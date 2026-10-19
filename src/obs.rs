//! obs-websocket v5, built into OBS 28 and later, as far as the watchdog reads it: connect, identify
//! (with the password when OBS asks for one), then requests one at a time. Read-only: [`Obs::request`]
//! sends nothing but `Get…` requests, so nothing here can change a scene, a source, a profile or the
//! recording.

use std::fmt;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

/// obs-websocket's close code for a password it refused, or none given when it asks for one.
const AUTHENTICATION_FAILED: u16 = 4009;

/// Why OBS couldn't be read, in the words health and the status use.
#[derive(Debug, Clone, PartialEq)]
pub enum ObsError {
    /// nothing answered on the port: OBS closed, or its WebSocket server off
    Unreachable(String),
    /// OBS asks for a password and the vault has none
    NeedsPassword,
    /// OBS refused every password the vault has
    WrongPassword,
    /// anything else: a request OBS refused, a dropped connection, a message that isn't obs-websocket's
    Other(String),
}

impl fmt::Display for ObsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ObsError::Unreachable(why) => write!(f, "nothing answered on OBS's WebSocket port ({why})"),
            ObsError::NeedsPassword => write!(f, "OBS's WebSocket server asks for a password: add it with /obs add"),
            ObsError::WrongPassword => write!(f, "OBS refused the WebSocket password: add the right one with /obs add"),
            ObsError::Other(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for ObsError {}

/// The answer to obs-websocket's challenge: base64(sha256(base64(sha256(password + salt)) + challenge)).
pub fn answer(password: &str, salt: &str, challenge: &str) -> String {
    let secret = BASE64.encode(Sha256::digest(format!("{password}{salt}")));
    BASE64.encode(Sha256::digest(format!("{secret}{challenge}")))
}

/// One identified connection to OBS.
pub struct Obs {
    ws: WebSocket<TcpStream>,
    next: u64,
    /// whether OBS asked for a password
    pub authenticated: bool,
}

impl fmt::Debug for Obs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Obs").field("authenticated", &self.authenticated).finish_non_exhaustive()
    }
}

fn other(what: &str, e: impl fmt::Display) -> ObsError {
    ObsError::Other(format!("{what}: {e}"))
}

impl Obs {
    /// Connects and identifies. `password` is only sent (as the challenge's answer) when OBS asks for one.
    pub fn connect(host: &str, port: u16, password: Option<&str>, timeout: Duration) -> Result<Obs, ObsError> {
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|e| ObsError::Unreachable(format!("{host}:{port}: {e}")))?
            .next()
            .ok_or_else(|| ObsError::Unreachable(format!("{host}:{port} has no address")))?;
        let tcp = TcpStream::connect_timeout(&addr, timeout).map_err(|e| ObsError::Unreachable(e.to_string()))?;
        tcp.set_read_timeout(Some(timeout)).and_then(|_| tcp.set_write_timeout(Some(timeout))).ok();
        let mut request = format!("ws://{host}:{port}").into_client_request().map_err(|e| other("the address", e))?;
        request.headers_mut().insert("Sec-WebSocket-Protocol", "obswebsocket.json".parse().expect("a header"));
        let (ws, _) = tungstenite::client(request, tcp).map_err(|e| other("OBS's WebSocket handshake", e))?;
        let mut obs = Obs { ws, next: 0, authenticated: false };

        let hello = obs.read_op(0)?;
        let auth = &hello["authentication"];
        let mut identify = json!({"rpcVersion": 1, "eventSubscriptions": 0});
        if auth.is_object() {
            obs.authenticated = true;
            let Some(password) = password else { return Err(ObsError::NeedsPassword) };
            let (salt, challenge) = (auth["salt"].as_str().unwrap_or(""), auth["challenge"].as_str().unwrap_or(""));
            identify["authentication"] = answer(password, salt, challenge).into();
        }
        obs.send(1, identify)?;
        match obs.read_op(2) {
            Ok(_) => Ok(obs),
            Err(ObsError::Other(why)) if why.contains(&format!("({AUTHENTICATION_FAILED})")) => {
                Err(ObsError::WrongPassword)
            }
            Err(e) => Err(e),
        }
    }

    fn send(&mut self, op: u64, d: Value) -> Result<(), ObsError> {
        let text = json!({"op": op, "d": d}).to_string();
        self.ws.send(Message::text(text)).map_err(|e| other("sending to OBS", e))
    }

    /// The `d` of the next message with this op code; events (op 5) are passed over.
    fn read_op(&mut self, op: u64) -> Result<Value, ObsError> {
        loop {
            let msg = match self.ws.read() {
                Ok(msg) => msg,
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    return Err(ObsError::Other("OBS didn't answer in time".into()))
                }
                Err(e) => return Err(other("reading from OBS", e)),
            };
            match msg {
                Message::Text(text) => {
                    let v: Value = serde_json::from_str(text.as_str()).map_err(|e| other("OBS's message", e))?;
                    if v["op"].as_u64() == Some(op) {
                        return Ok(v["d"].clone());
                    }
                }
                Message::Close(frame) => {
                    let (code, reason) = frame.map(|f| (u16::from(f.code), f.reason.to_string())).unwrap_or_default();
                    return Err(ObsError::Other(format!("OBS closed the connection ({code}): {reason}")));
                }
                _ => {}
            }
        }
    }

    /// A read of OBS: `kind` must be a `Get…` request. Its `responseData`, or why OBS refused it.
    pub fn request(&mut self, kind: &str, data: Value) -> Result<Value, ObsError> {
        if !kind.starts_with("Get") {
            return Err(ObsError::Other(format!("{kind} isn't a read: the watchdog only reads OBS")));
        }
        self.next += 1;
        let id = format!("r{}", self.next);
        self.send(6, json!({"requestType": kind, "requestId": id, "requestData": data}))?;
        loop {
            let d = self.read_op(7)?;
            if d["requestId"].as_str() != Some(&id) {
                continue;
            }
            let status = &d["requestStatus"];
            if status["result"].as_bool() == Some(true) {
                return Ok(d.get("responseData").cloned().unwrap_or_else(|| json!({})));
            }
            let comment = status["comment"].as_str().unwrap_or("no reason given");
            return Err(ObsError::Other(format!("OBS refused {kind} ({}): {comment}", status["code"])));
        }
    }

    /// A source's picture now, as PNG bytes, `width` across (its own shape kept).
    pub fn screenshot(&mut self, source: &str, width: u32) -> Result<Vec<u8>, ObsError> {
        let got = self
            .request("GetSourceScreenshot", json!({"sourceName": source, "imageFormat": "png", "imageWidth": width}))?;
        let data = got["imageData"].as_str().unwrap_or_default();
        let b64 = data.split_once(',').map_or(data, |(_, b)| b);
        BASE64.decode(b64).map_err(|e| other("OBS's screenshot", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_is_obs_websockets() {
        // from obs-websocket's protocol docs: password "supersecretpassword"
        let got = answer(
            "supersecretpassword",
            "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=",
            "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=",
        );
        assert_eq!(got, "1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu4=");
    }
}

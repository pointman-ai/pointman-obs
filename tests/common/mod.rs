//! What the tests share: obs under the SDK's stand-in node, against a fake OBS (an obs-websocket v5
//! server on a port of its own, answering the Get… requests from a state the test sets) and a fake
//! screen (the displays from a file, POINTMAN_OBS_FAKE, and a screencapture that copies the picture the
//! test puts in place), so no test reads this machine's OBS or screen. The node's `files.publish` is
//! answered here and kept, so a test sees each post the watchdog makes.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use pointman_extension::convert::whole_numbers;
use pointman_extension::testing::StandIn;
use serde_json::{json, Value};
use tempfile::TempDir;
use tungstenite::handshake::server::{Callback, ErrorResponse, Request, Response};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::CloseFrame;
use tungstenite::Message;

pub const ROOT: &str = env!("CARGO_MANIFEST_DIR");
pub const THREAD: i64 = 77;

/// A grey picture with a white square at (x, y), `side` pixels across, as PNG bytes.
pub fn picture(width: u32, height: u32, square: Option<(u32, u32, u32)>) -> Vec<u8> {
    let mut px = vec![60u8; (width * height * 3) as usize];
    if let Some((sx, sy, side)) = square {
        for y in sy..(sy + side).min(height) {
            for x in sx..(sx + side).min(width) {
                let i = ((y * width + x) * 3) as usize;
                px[i..i + 3].copy_from_slice(&[250, 250, 250]);
            }
        }
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&px).unwrap();
    out
}

/// What the fake OBS has: its password, the recording, the program scene's items, each input's kind and
/// settings, and each source's picture.
pub struct ObsState {
    pub password: Option<String>,
    pub recording: bool,
    pub paused: bool,
    pub bytes: u64,
    pub started: Instant,
    pub scene: String,
    pub items: Vec<Value>,
    pub inputs: HashMap<String, (String, Value)>,
    pub pictures: HashMap<String, Vec<u8>>,
    /// screenshots to refuse, once each: (source, width asked for)
    pub fail: Vec<(String, u64)>,
    /// whether the recording file grows at each GetRecordStatus
    pub grow: bool,
    /// every request's type, in order
    pub asked: Vec<String>,
}

impl ObsState {
    /// Recording a scene "Main" with one macOS Screen Capture of the main display, "Display Capture".
    pub fn recording() -> ObsState {
        ObsState {
            password: None,
            recording: true,
            paused: false,
            bytes: 0,
            started: Instant::now(),
            scene: "Main".into(),
            items: vec![
                json!({"sceneItemId": 1, "sourceName": "Display Capture", "inputKind": "screen_capture",
                       "isGroup": false, "sceneItemEnabled": true, "sourceType": "OBS_SOURCE_TYPE_INPUT"}),
                json!({"sceneItemId": 2, "sourceName": "Mic", "inputKind": "coreaudio_input_capture",
                       "isGroup": false, "sceneItemEnabled": true, "sourceType": "OBS_SOURCE_TYPE_INPUT"}),
            ],
            inputs: HashMap::from([
                ("Display Capture".to_string(), ("screen_capture".to_string(), json!({"display_uuid": "MAIN-UUID"}))),
                ("Mic".to_string(), ("coreaudio_input_capture".to_string(), json!({}))),
            ]),
            pictures: HashMap::from([
                ("Display Capture".to_string(), picture(640, 400, Some((100, 100, 40)))),
                ("Main".to_string(), picture(640, 400, Some((200, 200, 40)))),
            ]),
            fail: Vec::new(),
            grow: true,
            asked: Vec::new(),
        }
    }
}

pub struct FakeObs {
    pub port: u16,
    pub state: Arc<Mutex<ObsState>>,
}

impl FakeObs {
    pub fn start(state: ObsState) -> FakeObs {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(state));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || serve(stream, state));
            }
        });
        FakeObs { port, state }
    }

    pub fn set(&self, f: impl FnOnce(&mut ObsState)) {
        f(&mut self.state.lock().unwrap());
    }

    pub fn asked(&self) -> Vec<String> {
        self.state.lock().unwrap().asked.clone()
    }
}

/// A port nothing listens on: OBS closed, or its WebSocket server off.
pub fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

const SALT: &str = "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=";
const CHALLENGE: &str = "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=";

fn send(ws: &mut tungstenite::WebSocket<TcpStream>, op: u64, d: Value) -> bool {
    ws.send(Message::text(json!({"op": op, "d": d}).to_string())).is_ok()
}

/// obs-websocket answers with the subprotocol the client asks for (obswebsocket.json).
struct Agree;

impl Callback for Agree {
    fn on_request(self, req: &Request, mut res: Response) -> Result<Response, ErrorResponse> {
        if let Some(p) = req.headers().get("Sec-WebSocket-Protocol") {
            res.headers_mut().insert("Sec-WebSocket-Protocol", p.clone());
        }
        Ok(res)
    }
}

fn serve(stream: TcpStream, state: Arc<Mutex<ObsState>>) {
    let Ok(mut ws) = tungstenite::accept_hdr(stream, Agree) else { return };
    let password = state.lock().unwrap().password.clone();
    let mut hello = json!({"obsWebSocketVersion": "5.6.2", "rpcVersion": 1});
    if password.is_some() {
        hello["authentication"] = json!({"challenge": CHALLENGE, "salt": SALT});
    }
    if !send(&mut ws, 0, hello) {
        return;
    }
    loop {
        let Ok(msg) = ws.read() else { return };
        let Message::Text(text) = msg else { continue };
        let v: Value = serde_json::from_str(text.as_str()).unwrap();
        let d = &v["d"];
        match v["op"].as_u64() {
            Some(1) => {
                let want = password.as_ref().map(|p| obs::obs::answer(p, SALT, CHALLENGE));
                if want.is_some() && d["authentication"].as_str() != want.as_deref() {
                    let frame = CloseFrame { code: CloseCode::from(4009), reason: "Authentication failed.".into() };
                    let _ = ws.close(Some(frame));
                    let _ = ws.flush();
                    while ws.read().is_ok() {}
                    return;
                }
                if !send(&mut ws, 2, json!({"negotiatedRpcVersion": 1})) {
                    return;
                }
            }
            Some(6) => {
                let kind = d["requestType"].as_str().unwrap_or_default().to_string();
                let (ok, data) = answer(&mut state.lock().unwrap(), &kind, &d["requestData"]);
                let status = if ok {
                    json!({"result": true, "code": 100})
                } else {
                    json!({"result": false, "code": 600, "comment": data.as_str().unwrap_or("no")})
                };
                let reply = json!({"requestType": kind, "requestId": d["requestId"], "requestStatus": status,
                                   "responseData": if ok { data } else { Value::Null }});
                if !send(&mut ws, 7, reply) {
                    return;
                }
            }
            _ => {}
        }
    }
}

fn answer(st: &mut ObsState, kind: &str, data: &Value) -> (bool, Value) {
    st.asked.push(kind.to_string());
    let elapsed = st.started.elapsed().as_secs();
    match kind {
        "GetVersion" => (true, json!({"obsVersion": "32.2.2", "obsWebSocketVersion": "5.6.2"})),
        "GetRecordStatus" => {
            if st.recording && !st.paused && st.grow {
                st.bytes += 400_000;
            }
            (
                true,
                json!({"outputActive": st.recording, "outputPaused": st.paused,
                          "outputTimecode": format!("00:{:02}:{:02}.000", elapsed / 60, elapsed % 60),
                          "outputDuration": elapsed * 1000, "outputBytes": st.bytes}),
            )
        }
        "GetCurrentProgramScene" => (true, json!({"currentProgramSceneName": st.scene, "sceneName": st.scene})),
        "GetSceneItemList" | "GetGroupSceneItemList" => (true, json!({"sceneItems": st.items})),
        "GetInputSettings" => {
            let name = data["inputName"].as_str().unwrap_or_default();
            match st.inputs.get(name) {
                Some((kind, settings)) => (true, json!({"inputKind": kind, "inputSettings": settings})),
                None => (false, json!("No source was found by the name")),
            }
        }
        "GetSourceScreenshot" => {
            let name = data["sourceName"].as_str().unwrap_or_default();
            let width = data["imageWidth"].as_u64().unwrap_or(0);
            if let Some(i) = st.fail.iter().position(|(n, w)| n == name && *w == width) {
                st.fail.remove(i);
                return (false, json!("Failed to render screenshot"));
            }
            // the looks' 640 px picture is the test's; any other width (a post's) is a plain one that wide
            let sized = (width != 640).then(|| picture(width as u32, width as u32 * 5 / 8, None));
            match sized.as_ref().or(st.pictures.get(name)) {
                Some(png) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
                    (true, json!({"imageData": format!("data:image/png;base64,{b64}")}))
                }
                None => (false, json!("Failed to render screenshot")),
            }
        }
        _ => (false, json!(format!("the fake OBS doesn't know {kind}"))),
    }
}

/// The fake screen: one display (MAIN-UUID), the picture screencapture copies, and OBS's process.
pub struct FakeScreen {
    pub dir: TempDir,
}

pub const SCREENCAPTURE: &str = r#"#!/bin/sh
echo "screencapture $*" >> "$POINTMAN_OBS_TEST_CALLS"
for last; do :; done
cp "$POINTMAN_OBS_TEST_SCREEN" "$last"
"#;

impl FakeScreen {
    pub fn new() -> FakeScreen {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("screencapture");
        std::fs::write(&tool, SCREENCAPTURE).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let screen = FakeScreen { dir };
        screen.list(json!({"obs_pid": 4242}));
        screen.show(&picture(1512, 982, Some((10, 10, 60))));
        screen
    }

    /// The displays and OBS's process: `extra`'s keys over one main display.
    pub fn list(&self, extra: Value) {
        let mut v = json!({"displays": [{"id": 1, "uuid": "MAIN-UUID", "x": 0, "y": 0, "width": 1512, "height": 982,
                                         "main": true}],
                           "obs_windows": [], "locked": false});
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        let tmp = self.dir.path().join("list.tmp");
        std::fs::write(&tmp, v.to_string()).unwrap();
        std::fs::rename(tmp, self.dir.path().join("list.json")).unwrap();
    }

    /// What the real screen shows now.
    pub fn show(&self, png: &[u8]) {
        let tmp = self.dir.path().join("screen.tmp");
        std::fs::write(&tmp, png).unwrap();
        std::fs::rename(tmp, self.dir.path().join("screen.png")).unwrap();
    }

    pub fn calls(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// A screen that keeps moving: a square sliding across, a new place every 200 ms, until dropped.
pub struct Moving {
    stop: Arc<Mutex<bool>>,
}

impl Moving {
    pub fn start(screen: &FakeScreen) -> Moving {
        let stop = Arc::new(Mutex::new(false));
        let (flag, dir) = (stop.clone(), screen.dir.path().to_path_buf());
        std::thread::spawn(move || {
            let mut n = 0;
            while !*flag.lock().unwrap() {
                let png = picture(1512, 982, Some((20 + (n % 30) * 40, 300, 80)));
                let tmp = dir.join("moving.tmp");
                std::fs::write(&tmp, png).unwrap();
                std::fs::rename(tmp, dir.join("screen.png")).unwrap();
                n += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        Moving { stop }
    }
}

impl Drop for Moving {
    fn drop(&mut self) {
        *self.stop.lock().unwrap() = true;
    }
}

/// The posts the watchdog made: each `files.publish` body.
pub type Posts = Arc<Mutex<Vec<Value>>>;

/// obs under the stand-in node, against `port`, looking every second and frozen after 3.
pub fn obs(port: u16, screen: &FakeScreen, posts: &Posts, extra: Value) -> StandIn {
    let mut settings =
        json!({"port": port, "thread": THREAD, "mention": "@james", "every_seconds": 1, "frozen_seconds": 3});
    for (k, v) in extra.as_object().unwrap() {
        settings[k] = v.clone();
    }
    let kept = posts.clone();
    StandIn::new(ROOT)
        .binary(env!("CARGO_BIN_EXE_obs"))
        .settings(settings)
        .env("POINTMAN_OBS_FAKE", screen.path("list.json"))
        .env("POINTMAN_OBS_SCREENCAPTURE", screen.path("screencapture"))
        .env("POINTMAN_OBS_TEST_SCREEN", screen.path("screen.png"))
        .env("POINTMAN_OBS_TEST_CALLS", screen.path("calls"))
        .method("files.publish", move |p: Value| {
            let body = whole_numbers(p["body"].clone());
            let file = body["files"][0]["file"].as_str().unwrap_or_default().to_string();
            let mut b = body.clone();
            let png = std::fs::read(&file).unwrap_or_default();
            b["png"] = json!(png.starts_with(b"\x89PNG"));
            b["png_width"] = json!(png.get(16..20).map(|w| u32::from_be_bytes(w.try_into().unwrap())));
            let mut kept = kept.lock().unwrap();
            kept.push(b);
            let n = kept.len();
            Ok(json!({"file": {"value": "5"}, "version": {"value": n.to_string()}, "post": {"value": (900 + n).to_string()}}))
        })
}

/// Waits until `test` holds of the posts, or `within` passes.
pub fn wait_posts(posts: &Posts, within: Duration, test: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let until = Instant::now() + within;
    loop {
        let got = posts.lock().unwrap().clone();
        if test(&got) || Instant::now() > until {
            return got;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn text(post: &Value) -> String {
    post["post"]["text"].as_str().unwrap_or_default().to_string()
}

pub fn part<'a>(parts: &'a Value, id: &str) -> &'a Value {
    parts["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .unwrap_or_else(|| panic!("no part {id} in {parts}"))
}

pub fn exists(p: &Path) -> bool {
    p.exists()
}

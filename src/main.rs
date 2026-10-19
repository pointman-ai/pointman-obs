//! The extension's process: the node starts it as extension.toml's `[run]` says (`bin/obs`) and talks to
//! it over stdin and stdout (the SDK handles that). Log with `h.log(…)`: stdout is the node's.
//!
//! Every half second on the `obs` lane it checks whether a look is due (`every_seconds`). A look, while
//! OBS records: the real screen first, then each watched source's picture from OBS, compared with the
//! last look's (watch.rs judges). An alert, its all-clear and the rest go out as a `files.publish` of the
//! source's picture with a post in the settings' thread, from a thread of their own so a slow upload
//! never holds up a look. Open alerts are kept in the state folder, so a restart doesn't alert twice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use obs::frames::{Area, Grid};
use obs::obs::{Obs, ObsError};
use obs::screen::{self, Real, Screen};
use obs::sources::{self, Shows, Source};
use obs::watch::{Growth, Look, Mark, Stall, Verdict, Watch};
use obs::words;
use pointman_extension::{Extension, Handle, Job};
use serde_json::{json, Value};

/// How often a look is checked for.
const TICK: Duration = Duration::from_millis(500);
/// How long OBS has to answer.
const OBS_TIMEOUT: Duration = Duration::from_secs(10);
/// Asking again after OBS didn't answer, or refused the password.
const UNREACHABLE_AGAIN: Duration = Duration::from_secs(10);
const PASSWORD_AGAIN: Duration = Duration::from_secs(60);
/// OBS's picture across, to compare; and for the post.
const LOOK_WIDTH: u32 = 640;
const POST_WIDTH: u32 = 1280;
/// A picture moved when any cell moved by more than this (of 255). A frozen capture's is the same
/// texture each time, so its cells don't move at all.
const PICTURE_THRESHOLD: u8 = 4;
/// The screen moved when this share of its cells moved by more than SCREEN_THRESHOLD: a window, a
/// scroll, a line or two of typing, not a clock's tick. OBS's picture of a live capture shows such a
/// move in many more cells than PICTURE_THRESHOLD needs, so a live capture is never taken for a frozen one.
const SCREEN_THRESHOLD: u8 = 12;
const SCREEN_MOVED: f64 = 0.0005;
/// Pictures kept for posts that may still be going out.
const POSTS_KEPT: usize = 40;

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// This machine's settings (config.toml), with the manifest's defaults.
struct Settings {
    host: String,
    port: u16,
    thread: Option<i64>,
    mention: String,
    every: f64,
    frozen: f64,
    source: String,
}

fn settings(h: &Handle) -> Settings {
    let s = h.settings();
    let text = |k: &str, d: &str| s[k].as_str().map(str::trim).unwrap_or(d).to_string();
    Settings {
        host: text("host", "127.0.0.1"),
        port: s["port"].as_u64().unwrap_or(4455) as u16,
        thread: s["thread"].as_i64().filter(|t| *t > 0),
        mention: text("mention", ""),
        every: s["every_seconds"].as_f64().unwrap_or(15.0).max(1.0),
        frozen: s["frozen_seconds"].as_f64().unwrap_or(60.0).max(2.0),
        source: text("source", ""),
    }
}

/// One source being watched: its watch, and the last look's pictures.
struct Seen {
    watch: Watch,
    shows: String,
    picture: Option<Grid>,
    screen: Option<Grid>,
    masks: Vec<Area>,
    note: Option<String>,
}

/// An alert that's out: from when, its card, and the picture it went with.
#[derive(Clone)]
struct Open {
    since: Mark,
    asset: String,
    image: PathBuf,
}

fn open_json(o: &Open) -> Value {
    json!({"at": o.since.at, "timecode": o.since.timecode, "asset": o.asset, "image": o.image})
}

fn open_from(v: &Value) -> Option<Open> {
    Some(Open {
        since: Mark { at: v["at"].as_f64()?, timecode: v["timecode"].as_str()?.to_string() },
        asset: v["asset"].as_str()?.to_string(),
        image: PathBuf::from(v["image"].as_str()?),
    })
}

/// What health and obs.status say: the last look's.
#[derive(Default, Clone)]
struct Status {
    looked: bool,
    running: bool,
    problem: Option<ObsError>,
    version: Option<String>,
    recording: Option<Value>,
    scene: Option<String>,
    /// (source, what it shows, its state)
    watching: Vec<(String, String, String)>,
    frozen: Vec<(String, Mark)>,
    stalled: Option<Mark>,
    thread: Option<i64>,
    mention: String,
    port: u16,
    posted: Option<String>,
}

/// A post to make: a picture on a card, and the text in the thread.
struct Post {
    thread: i64,
    asset: String,
    title: String,
    text: String,
    image: PathBuf,
}

struct Dog {
    h: Handle,
    obs: Option<Obs>,
    password: Option<String>,
    next: Instant,
    seen: HashMap<String, Seen>,
    growth: Growth,
    open: HashMap<String, Open>,
    status: Arc<Mutex<Status>>,
    posts: Sender<Post>,
    state: PathBuf,
    machine: String,
}

/// The key a stalled recording's alert is kept under, beside the sources'.
const STALL: &str = "\u{0}recording";

impl Dog {
    fn new(h: Handle, status: Arc<Mutex<Status>>, posts: Sender<Post>) -> Dog {
        Dog {
            h,
            obs: None,
            password: None,
            next: Instant::now(),
            seen: HashMap::new(),
            growth: Growth::default(),
            open: HashMap::new(),
            status,
            posts,
            state: PathBuf::new(),
            machine: String::new(),
        }
    }

    /// The node's folder and machine name (initialize's, so not before the first look), and the alerts
    /// still out from before a restart.
    fn ready(&mut self) {
        if !self.machine.is_empty() {
            return;
        }
        let node = self.h.node();
        self.state = node["state_dir"].as_str().map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        self.machine = node["machine"].as_str().filter(|m| !m.is_empty()).unwrap_or("this machine").to_string();
        self.open = std::fs::read_to_string(self.state.join("alerts.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_object().cloned())
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), open_from(v)?))).collect())
            .unwrap_or_default();
    }

    fn save_open(&self) {
        let v: serde_json::Map<String, Value> = self.open.iter().map(|(k, o)| (k.clone(), open_json(o))).collect();
        if let Err(e) = std::fs::write(self.state.join("alerts.json"), Value::Object(v).to_string()) {
            self.h.log(format!("couldn't keep the open alerts: {e}"));
        }
    }

    /// Connected to OBS, with the vault's password when it asks for one (each `obs.` item in turn, until
    /// one works).
    fn connect(&mut self, s: &Settings) -> Result<&mut Obs, ObsError> {
        if self.obs.is_none() {
            let obs = match Obs::connect(&s.host, s.port, self.password.as_deref(), OBS_TIMEOUT) {
                Err(ObsError::NeedsPassword | ObsError::WrongPassword) => self.with_vault(s)?,
                got => got?,
            };
            self.obs = Some(obs);
        }
        Ok(self.obs.as_mut().expect("connected"))
    }

    fn with_vault(&mut self, s: &Settings) -> Result<Obs, ObsError> {
        self.password = None;
        let listed =
            self.h.request("secret.list", json!({"prefix": "obs."})).map_err(|e| ObsError::Other(format!("{e:#}")))?;
        let names: Vec<String> =
            listed["names"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect();
        if names.is_empty() {
            return Err(ObsError::NeedsPassword);
        }
        for name in names {
            let password = match self.h.secret(&name) {
                Ok(p) => p.trim().to_string(),
                Err(e) => {
                    self.h.log(format!("couldn't open {name}: {e:#}"));
                    continue;
                }
            };
            match Obs::connect(&s.host, s.port, Some(&password), OBS_TIMEOUT) {
                Ok(obs) => {
                    self.password = Some(password);
                    return Ok(obs);
                }
                Err(ObsError::WrongPassword) => self.h.log(format!("OBS refused the password in {name}")),
                Err(e) => return Err(e),
            }
        }
        Err(ObsError::WrongPassword)
    }

    fn tick(&mut self) {
        self.ready();
        let started = Instant::now();
        if started < self.next {
            return;
        }
        let s = settings(&self.h);
        self.next = started + Duration::from_secs_f64(s.every);
        let looked = self.look(&s);
        let running = screen::obs_pid().is_some();
        if let Err(e) = &looked {
            self.obs = None;
            match e {
                ObsError::Unreachable(_) => self.next = self.next.max(started + UNREACHABLE_AGAIN),
                ObsError::NeedsPassword | ObsError::WrongPassword => self.next = started + PASSWORD_AGAIN,
                ObsError::Other(why) => self.h.log(format!("a look at OBS failed: {why}")),
            }
            if !running {
                self.finish("ended when OBS closed", &s);
            }
        }
        let mut st = lock(&self.status);
        st.looked = true;
        st.running = running || looked.is_ok();
        st.problem = looked.err();
        st.thread = s.thread;
        st.mention = s.mention.clone();
        st.port = s.port;
        st.frozen = self.seen.iter().filter_map(|(n, w)| Some((n.clone(), w.watch.alerted.clone()?))).collect();
        st.stalled = self.growth.alerted.clone();
        st.watching = self.watching();
        let event = json!({
            "running": st.running,
            "connected": st.problem.is_none(),
            "recording": st.recording.as_ref().is_some_and(|r| r["outputActive"] == true),
            "scene": st.scene,
            "watching": st.watching.iter().map(|w| &w.0).collect::<Vec<_>>(),
            "frozen": st.frozen.iter().map(|f| &f.0).collect::<Vec<_>>(),
            "problem": st.problem.as_ref().map(|p| p.to_string()),
        });
        drop(st);
        self.h.event("obs.status", event);
    }

    fn watching(&self) -> Vec<(String, String, String)> {
        let mut out: Vec<_> = self
            .seen
            .iter()
            .map(|(name, w)| {
                let state = match (&w.watch.alerted, &w.watch.still_since, &w.note) {
                    (Some(since), _, _) => format!("frozen since {}", words::clock(since.at)),
                    (None, _, Some(note)) => note.clone(),
                    (None, Some(since), None) if now() - since.at >= 2.0 => {
                        format!("still since {} (the screen too)", words::clock(since.at))
                    }
                    _ => "moving".to_string(),
                };
                (name.clone(), w.shows.clone(), state)
            })
            .collect();
        out.sort();
        out
    }

    fn look(&mut self, s: &Settings) -> Result<(), ObsError> {
        let obs = self.connect(s)?;
        let version = obs.request("GetVersion", json!({}))?["obsVersion"].as_str().map(String::from);
        let rec = obs.request("GetRecordStatus", json!({}))?;
        {
            let mut st = lock(&self.status);
            st.version = version;
            st.recording = Some(rec.clone());
        }
        let at = Mark { at: now(), timecode: rec["outputTimecode"].as_str().unwrap_or("").to_string() };
        if rec["outputActive"] != true {
            self.finish("stopped", s);
            return Ok(());
        }
        if rec["outputPaused"] == true {
            // nothing's written while paused: each watch starts afresh after it
            for w in self.seen.values_mut() {
                w.picture = None;
                w.screen = None;
            }
            return Ok(());
        }
        let (scene, found) = sources::find(self.obs.as_mut().expect("connected"), &s.source)?;
        lock(&self.status).scene = Some(scene.clone());
        let pid = screen::obs_pid();
        let screen = screen::look(pid).unwrap_or_else(|e| {
            self.h.log(format!("couldn't read the screen: {e:#}"));
            Screen::default()
        });
        for source in &found {
            self.look_at(source, &screen, &at, s)?;
        }
        let gone: Vec<String> = self.seen.keys().filter(|n| found.iter().all(|f| &f.name != *n)).cloned().collect();
        for name in gone {
            self.seen.remove(&name);
            if let Some(open) = self.open.remove(&name) {
                self.post(s, &open, words::gone(&name, &scene, &open.since, &at), None);
                self.save_open();
            }
        }
        let bytes = rec["outputBytes"].as_u64().unwrap_or(0);
        match self.growth.look(&at, bytes, s.frozen) {
            Some(Stall::Stalled { since, bytes }) => {
                let text = words::stalled(&s.mention, &self.machine, &since, bytes);
                match self.picture_for_post(&scene, "recording", None) {
                    Some(image) => {
                        let open = Open { asset: self.asset("recording", &since), since, image };
                        self.post(s, &open, text, Some("Recording stalled"));
                        self.open.insert(STALL.into(), open);
                        self.save_open();
                    }
                    // nothing to post it with: the next look alerts again
                    None => self.growth.alerted = None,
                }
            }
            Some(Stall::Growing { since, at }) => {
                if let Some(open) = self.open.remove(STALL) {
                    self.post(s, &open, words::growing(&since, &at), None);
                    self.save_open();
                }
            }
            None => {}
        }
        Ok(())
    }

    /// One source: the real screen first, then OBS's picture, so a move landing between the two shows
    /// in OBS's and is never taken for a freeze.
    fn look_at(&mut self, source: &Source, screen: &Screen, at: &Mark, s: &Settings) -> Result<(), ObsError> {
        let open = self.open.get(&source.name).cloned();
        let seen = self.seen.entry(source.name.clone()).or_insert_with(|| Seen {
            watch: open.as_ref().map(|o| Watch::alerted(o.since.clone())).unwrap_or_default(),
            shows: String::new(),
            picture: None,
            screen: None,
            masks: Vec::new(),
            note: None,
        });
        let (real, note) = match &source.shows {
            Shows::Display { uuid, id, index } => match screen::find(screen, uuid.as_deref(), *id, *index) {
                Some(d) if d.asleep => (Some(Real::Display(d)), Some("its display is asleep".to_string())),
                Some(d) => (Some(Real::Display(d)), None),
                None if screen.displays.is_empty() => (None, Some("the screen couldn't be read".to_string())),
                None => (None, Some("its display isn't connected".to_string())),
            },
            Shows::Window(id) => (Some(Real::Window(*id)), None),
            Shows::Nothing(why) => (None, Some(why.clone())),
        };
        seen.shows = real.as_ref().map(Real::describe).unwrap_or_else(|| "nothing to compare".into());
        let note = note.or_else(|| screen.locked.then(|| "the screen is locked".to_string()));
        let mut taken = None;
        let mut masks = Vec::new();
        if let (Some(real), None) = (&real, &note) {
            let file = self.state.join("screen.png");
            let got = screen::take(real, &file).and_then(|_| Grid::from_png(&std::fs::read(&file)?));
            let _ = std::fs::remove_file(&file);
            match got {
                Ok(grid) => taken = Some(grid),
                Err(e) => self.h.log(format!("{}: {e:#}", source.name)),
            }
            if let Real::Display(d) = real {
                masks = screen::masks(d, &screen.obs_windows);
            }
        }
        let obs = self.obs.as_mut().expect("connected");
        let png = match obs.screenshot(&source.name, LOOK_WIDTH) {
            Ok(png) => png,
            Err(ObsError::Other(why)) if why.starts_with("OBS refused") => {
                let seen = self.seen.get_mut(&source.name).expect("seen");
                seen.note = Some(format!("OBS gave no picture of it: {why}"));
                seen.picture = None;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let picture = Grid::from_png(&png).map_err(|e| ObsError::Other(format!("{}'s picture: {e:#}", source.name)))?;
        let seen = self.seen.get_mut(&source.name).expect("seen");
        seen.note = note;
        let picture_moved = seen.picture.as_ref().map(|p| picture.changed(p, PICTURE_THRESHOLD, &[]).cells > 0);
        let both: Vec<Area> = masks.iter().chain(&seen.masks).copied().collect();
        let screen_moved = match (&seen.screen, &taken) {
            (Some(before), Some(now)) => Some(now.changed(before, SCREEN_THRESHOLD, &both).fraction() >= SCREEN_MOVED),
            _ => None,
        };
        seen.picture = Some(picture);
        seen.screen = taken;
        seen.masks = masks;
        let look = Look { at: at.clone(), picture_moved, screen_moved };
        match seen.watch.look(&look, s.frozen) {
            Some(Verdict::Frozen { since }) => {
                let text =
                    words::frozen(&s.mention, &self.machine, &source.name, &seen.shows, &since, self.growth.rate());
                match self.picture_for_post(&source.name, &source.name, Some(&png)) {
                    Some(image) => {
                        let open = Open { asset: self.asset(&source.name, &since), since, image };
                        self.post(s, &open, text, Some(&format!("{} frozen", source.name)));
                        self.open.insert(source.name.clone(), open);
                        self.save_open();
                    }
                    // nothing to post it with: the next look alerts again
                    None => self.seen.get_mut(&source.name).expect("seen").watch.alerted = None,
                }
            }
            Some(Verdict::Moving { since, at }) => {
                let open = self.open.remove(&source.name);
                let image = self.picture_for_post(&source.name, &source.name, Some(&png));
                if let Some(mut open) = open {
                    open.image = image.unwrap_or(open.image);
                    self.post(s, &open, words::moving(&source.name, &since, &at), None);
                    self.save_open();
                }
            }
            None => {}
        }
        Ok(())
    }

    /// The card a freeze's posts go on: one per freeze, its all-clear the card's next version.
    fn asset(&self, name: &str, since: &Mark) -> String {
        format!("obs/{}/{}/{}", words::slug(&self.machine), words::slug(name), words::stamp(since.at))
    }

    /// OBS's picture of a source (or a scene) for a post, kept in the state folder until it's gone out.
    /// If OBS doesn't give one, `fallback` (the look's own, smaller); with neither, None.
    fn picture_for_post(&mut self, source: &str, what: &str, fallback: Option<&[u8]>) -> Option<PathBuf> {
        let dir = self.state.join("posts");
        std::fs::create_dir_all(&dir).ok()?;
        prune(&dir, POSTS_KEPT);
        let path = dir.join(format!("{}-{}.png", words::slug(what), words::stamp(now()).replace('-', "")));
        let png = match self.obs.as_mut().map(|obs| obs.screenshot(source, POST_WIDTH)) {
            Some(Ok(png)) => png,
            got => {
                let why = match got {
                    Some(Err(e)) => e.to_string(),
                    _ => "not connected".into(),
                };
                match fallback {
                    Some(look) => {
                        self.h.log(format!("no {POST_WIDTH} px picture of {source} ({why}): posting the look's"));
                        look.to_vec()
                    }
                    None => {
                        self.h
                            .log(format!("no picture of {source} for the post ({why}): trying again at the next look"));
                        return None;
                    }
                }
            }
        };
        match std::fs::write(&path, png) {
            Ok(()) => Some(path),
            Err(e) => {
                self.h.log(format!("couldn't keep {source}'s picture for the post: {e}"));
                None
            }
        }
    }

    fn post(&self, s: &Settings, open: &Open, text: String, title: Option<&str>) {
        let Some(thread) = s.thread else {
            self.h.log(format!("no thread set for alerts, so this isn't posted: {text}"));
            return;
        };
        let title =
            title.map(String::from).unwrap_or_else(|| open.asset.rsplit('/').nth(1).unwrap_or("OBS").to_string());
        let _ = self.posts.send(Post { thread, asset: open.asset.clone(), title, text, image: open.image.clone() });
    }

    /// The recording stopped (or OBS closed): each open alert says so, once, and every watch ends.
    fn finish(&mut self, how: &str, s: &Settings) {
        let open: Vec<(String, Open)> = self.open.drain().collect();
        for (name, o) in &open {
            let text = if name == STALL {
                format!("The recording {how}; its file hadn't grown since {}.", words::timecode(&o.since.timecode))
            } else {
                words::ended(name, &o.since, how)
            };
            self.post(s, o, text, None);
        }
        if !open.is_empty() {
            self.save_open();
        }
        self.seen.clear();
        self.growth = Growth::default();
        lock(&self.status).scene = None;
    }
}

/// Keeps the newest `keep` files in `dir`.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, PathBuf)> =
        entries.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
    if files.len() <= keep {
        return;
    }
    files.sort();
    for (_, path) in &files[..files.len() - keep] {
        let _ = std::fs::remove_file(path);
    }
}

/// Posts go out one at a time, in order, each tried three times.
fn publisher(h: Handle, status: Arc<Mutex<Status>>) -> Sender<Post> {
    let (tx, rx) = mpsc::channel::<Post>();
    std::thread::spawn(move || {
        for post in rx {
            let name = post.image.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let body = json!({
                "project": {"slug": "obs", "name": "OBS"},
                "asset": {"path": post.asset, "title": post.title, "kind": "image"},
                "caption": post.text,
                "files": [{"role": "image", "file": post.image, "name": name, "mime": "image/png"}],
                "by": "obs",
                "post": {"thread_id": post.thread, "text": post.text},
            });
            let mut last = None;
            for attempt in 0..3 {
                if attempt > 0 {
                    std::thread::sleep(Duration::from_secs(10));
                }
                match h.request_timeout("files.publish", json!({"body": body}), Duration::from_secs(120)) {
                    Ok(answer) => {
                        h.log(format!(
                            "posted {} in thread {}: message {}",
                            post.asset, post.thread, answer["message_id"]
                        ));
                        last = None;
                        break;
                    }
                    Err(e) => last = Some(format!("{e:#}")),
                }
            }
            if let Some(e) = &last {
                h.log(format!("couldn't post {} in thread {}: {e}", post.asset, post.thread));
            }
            lock(&status).posted = last.map(|e| format!("the last post didn't go out: {e}"));
        }
    });
    tx
}

/// The two parts health shows: OBS itself, and the watchdog.
fn health(st: &Status) -> Value {
    let rec = st.recording.as_ref().filter(|r| r["outputActive"] == true);
    let (state, needs, why): (&str, Vec<&str>, Option<String>) = match (&st.problem, st.looked) {
        (_, false) => ("unknown", vec![], Some("not looked at yet".into())),
        (Some(ObsError::Unreachable(_)), _) if !st.running => ("stopped", vec![], Some("OBS isn't open".into())),
        (Some(ObsError::Unreachable(_)), _) => (
            "failing",
            vec![],
            Some(format!(
                "OBS is open, but nothing answers on its WebSocket port ({}): turn its server on in OBS › Tools › \
                 WebSocket Server Settings",
                st.port
            )),
        ),
        (Some(e @ (ObsError::NeedsPassword | ObsError::WrongPassword)), _) => {
            ("failing", vec!["auth"], Some(e.to_string()))
        }
        (Some(e), _) => ("failing", vec![], Some(e.to_string())),
        (None, true) => match rec {
            Some(r) => (
                "ok",
                vec![],
                Some(format!("recording, {} in", words::timecode(r["outputTimecode"].as_str().unwrap_or("")))),
            ),
            None => ("ok", vec![], Some("not recording".into())),
        },
    };
    let obs = json!({"id": "obs", "kind": "app", "name": "OBS Studio", "version": st.version,
                     "state": state, "needs": needs, "why": why});
    let watched = st.watching.iter().map(|w| w.0.as_str()).collect::<Vec<_>>().join(", ");
    let (state, why) = if let Some((name, since)) = st.frozen.first() {
        (
            "failing",
            format!(
                "{name} frozen since {} ({} into the recording)",
                words::clock(since.at),
                words::timecode(&since.timecode)
            ),
        )
    } else if let Some(since) = &st.stalled {
        ("failing", format!("the recording hasn't grown since {}", words::clock(since.at)))
    } else if st.thread.is_none() {
        ("failing", "no thread to post alerts in: set `thread` in ~/.config/pointman/extensions/obs/config.toml".into())
    } else if st.mention.is_empty() {
        (
            "failing",
            "alerts mention nobody, so nobody is told: set `mention` in ~/.config/pointman/extensions/obs/config.toml"
                .into(),
        )
    } else if let Some(posted) = &st.posted {
        ("failing", posted.clone())
    } else if st.problem.is_some() || !st.looked {
        ("stopped", "not watching: OBS can't be read".into())
    } else if rec.is_none() {
        ("ok", "not recording: nothing to watch".into())
    } else if watched.is_empty() {
        ("ok", "recording, but no screen capture is showing in the program scene".into())
    } else {
        ("ok", format!("watching {watched}"))
    };
    json!([obs, {"id": "watchdog", "kind": "watchdog", "name": "Recording watchdog", "state": state, "needs": [], "why": why}])
}

/// obs.status's answer, from a fresh read of OBS and the watchdog's last look.
fn status_text(st: &Status, machine: &str, mention: &str) -> String {
    let mut lines = Vec::new();
    match &st.problem {
        Some(ObsError::Unreachable(_)) if !st.running => lines.push(format!("OBS isn't open on {machine}.")),
        Some(ObsError::Unreachable(_)) => lines.push(format!(
            "OBS is open on {machine}, but nothing answers on its WebSocket port ({}): turn its server on in OBS › \
             Tools › WebSocket Server Settings.",
            st.port
        )),
        Some(e) => lines.push(format!("OBS on {machine}: {e}.")),
        None => {
            let version = st.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
            let rec = st.recording.as_ref().filter(|r| r["outputActive"] == true);
            let scene = st.scene.as_deref().map(|s| format!(", in scene “{s}”")).unwrap_or_default();
            lines.push(match rec {
                Some(r) if r["outputPaused"] == true => format!("OBS{version} on {machine}: recording, paused{scene}."),
                Some(r) => format!(
                    "OBS{version} on {machine}: recording, {} in ({}){scene}.",
                    words::timecode(r["outputTimecode"].as_str().unwrap_or("")),
                    words::size(r["outputBytes"].as_u64().unwrap_or(0))
                ),
                None => format!("OBS{version} on {machine}: not recording, so nothing is watched."),
            });
            if rec.is_some() {
                if st.watching.is_empty() {
                    lines.push("No screen capture is showing in the program scene, so nothing is watched.".into());
                }
                for (name, shows, state) in &st.watching {
                    lines.push(format!("Watching **{name}** ({shows}): {state}."));
                }
            }
        }
    }
    lines.push(match st.thread {
        Some(t) if mention.is_empty() => format!(
            "Alerts go to thread {t}, mentioning nobody, so nobody is told: set `mention` in \
             ~/.config/pointman/extensions/obs/config.toml."
        ),
        Some(t) => format!("Alerts go to thread {t}, mentioning {mention}."),
        None => "No thread is set for alerts: set `thread` in ~/.config/pointman/extensions/obs/config.toml.".into(),
    });
    lines.join("\n")
}

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;
    let h = ext.handle();
    let status = Arc::new(Mutex::new(Status::default()));
    let posts = publisher(h.clone(), status.clone());
    let dog = Arc::new(Mutex::new(Dog::new(h.clone(), status.clone(), posts)));

    let d = dog.clone();
    ext.every(TICK, move |_: &Handle| lock(&d).tick(), "obs");

    // a live read of OBS now: whether it's open, its WebSocket, the recording, and what's watched
    let d = dog.clone();
    let st = status.clone();
    ext.command("obs.status", move |_: &Job| {
        let mut dog = lock(&d);
        dog.next = Instant::now();
        dog.tick();
        let s = settings(&dog.h);
        let snapshot = lock(&st).clone();
        let text = status_text(&snapshot, &dog.machine, &s.mention);
        Ok(json!({
            "text": text,
            "running": snapshot.running,
            "connected": snapshot.problem.is_none(),
            "version": snapshot.version,
            "recording": snapshot.recording,
            "scene": snapshot.scene,
            "watching": snapshot.watching.iter().map(|(n, shows, state)| json!({"source": n, "shows": shows, "state": state})).collect::<Vec<_>>(),
            "thread": snapshot.thread,
        }))
    });

    // `/obs add` posted (an agent, an older app): the apps open the password's form for it
    ext.command("obs.add", |_: &Job| {
        Ok(json!({"text": "OBS's WebSocket password is added in the Pointman app, so it never goes in a post: type \
                           `/obs add` there and paste it. It's in OBS › Tools › WebSocket Server Settings › Show \
                           Connect Info."}))
    });

    let st = status.clone();
    ext.method("health.check", move |_| Ok(health(&lock(&st))));
    let st = status.clone();
    ext.method("health.fix", move |params: Value| {
        let message = match (params["part"].as_str(), params["action"].as_str()) {
            (Some("obs"), Some("auth")) => "Add OBS's WebSocket password with /obs add in any thread: the app opens a \
                                            form, so it never goes in a post. It's in OBS › Tools › WebSocket Server \
                                            Settings › Show Connect Info."
                .to_string(),
            _ => {
                let parts = health(&lock(&st));
                let why = parts
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|p| p["id"] == params["part"])
                    .map(|p| p["why"].clone());
                format!(
                    "Nothing to fix from here{}",
                    why.and_then(|w| w.as_str().map(|w| format!(": {w}"))).unwrap_or_default()
                )
            }
        };
        Ok(json!({"message": message}))
    });

    ext.run()
}

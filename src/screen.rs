//! The real screen, to hold OBS's picture against: a display or a window, taken with macOS's own
//! `screencapture`, and where OBS's own windows are on it. Those are left out of the comparison: OBS's
//! meters and timer move all the time, while a capture that hides OBS never shows them. A locked or
//! sleeping screen isn't taken at all.
//!
//! For tests, on any platform: `POINTMAN_OBS_FAKE` names a JSON file read instead of CoreGraphics
//! (`{displays: [{id, uuid, x, y, width, height, main, asleep}], obs_windows: [{x, y, width, height}],
//! locked, obs_pid}`, read afresh each time), and `POINTMAN_OBS_SCREENCAPTURE` a program run in
//! screencapture's place (its last argument is the PNG to write).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::frames::Area;

pub const FAKE_ENV: &str = "POINTMAN_OBS_FAKE";
pub const CAPTURE_ENV: &str = "POINTMAN_OBS_SCREENCAPTURE";

/// How long a screenshot may take before it's given up on.
const TAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A rectangle in global display points, from the top-left of the main display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    /// its CoreGraphics number
    pub id: u32,
    /// CGDisplayCreateUUIDFromDisplayID's, as OBS keeps it in a capture's `display_uuid`
    pub uuid: String,
    pub bounds: Rect,
    pub main: bool,
    pub asleep: bool,
}

/// The screen as it is now: its displays, OBS's windows on it, and whether it's locked.
#[derive(Debug, Clone, Default)]
pub struct Screen {
    pub displays: Vec<Display>,
    pub obs_windows: Vec<Rect>,
    pub locked: bool,
}

/// What on the real screen a capture shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Real {
    Display(Display),
    Window(u32),
}

impl Real {
    pub fn describe(&self) -> String {
        match self {
            Real::Display(d) if d.main => format!("display {} (the main one)", d.id),
            Real::Display(d) => format!("display {}", d.id),
            Real::Window(id) => format!("window {id}"),
        }
    }
}

fn fake() -> Option<Result<Value>> {
    let path = std::env::var_os(FAKE_ENV)?;
    let read = || -> Result<Value> {
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path:?}"))?;
        serde_json::from_str(&text).context("the fake screen isn't JSON")
    };
    Some(read())
}

fn rect(v: &Value) -> Rect {
    let n = |k: &str| v[k].as_f64().unwrap_or(0.0);
    Rect { x: n("x"), y: n("y"), width: n("width"), height: n("height") }
}

/// The OBS process, if it's running.
pub fn obs_pid() -> Option<i32> {
    if let Some(v) = fake() {
        return v.ok()?["obs_pid"].as_i64().map(|p| p as i32);
    }
    let name = if cfg!(target_os = "macos") { "OBS" } else { "obs" };
    let out = Command::new("pgrep").args(["-x", name]).stderr(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
}

/// The displays, OBS's windows (those of `obs`, its process) and whether the screen is locked.
pub fn look(obs: Option<i32>) -> Result<Screen> {
    if let Some(v) = fake() {
        let v = v?;
        let displays = v["displays"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, d)| Display {
                id: d["id"].as_u64().unwrap_or(i as u64 + 1) as u32,
                uuid: d["uuid"].as_str().unwrap_or_default().to_string(),
                bounds: rect(d),
                main: d["main"].as_bool().unwrap_or(i == 0),
                asleep: d["asleep"].as_bool().unwrap_or(false),
            })
            .collect();
        let obs_windows = v["obs_windows"].as_array().into_iter().flatten().map(rect).collect();
        return Ok(Screen { displays, obs_windows, locked: v["locked"].as_bool().unwrap_or(false) });
    }
    real(obs)
}

/// The display a capture shows, from its settings: by `display_uuid`, else by its CoreGraphics number
/// (`id`, the macOS Screen Capture's `display` before OBS 30) or its place in the list (`index`, the old
/// Display Capture's), else the main display.
pub fn find(screen: &Screen, uuid: Option<&str>, id: Option<u32>, index: Option<usize>) -> Option<Display> {
    let d = &screen.displays;
    let found = match (uuid.filter(|u| !u.is_empty()), id, index) {
        (Some(u), _, _) => d.iter().find(|x| x.uuid.eq_ignore_ascii_case(u)),
        (None, Some(id), _) => d.iter().find(|x| x.id == id),
        (None, None, Some(i)) => d.get(i),
        (None, None, None) => d.iter().find(|x| x.main).or(d.first()),
    };
    found.cloned()
}

/// Where OBS's windows are on a display, as parts of its picture.
pub fn masks(display: &Display, windows: &[Rect]) -> Vec<Area> {
    let b = display.bounds;
    if b.width <= 0.0 || b.height <= 0.0 {
        return Vec::new();
    }
    windows
        .iter()
        .filter_map(|w| {
            let (x0, y0) = (w.x.max(b.x), w.y.max(b.y));
            let (x1, y1) = ((w.x + w.width).min(b.x + b.width), (w.y + w.height).min(b.y + b.height));
            (x1 > x0 && y1 > y0).then(|| Area {
                x: (x0 - b.x) / b.width,
                y: (y0 - b.y) / b.height,
                width: (x1 - x0) / b.width,
                height: (y1 - y0) / b.height,
            })
        })
        .collect()
}

/// The parts of OBS's windows nothing covers, from the app windows front to back (whether each is
/// OBS's, and where): a covered OBS window hides nothing of what's in front of it.
pub fn visible(stack: &[(bool, Rect)]) -> Vec<Rect> {
    let mut out = Vec::new();
    for (i, &(obs, rect)) in stack.iter().enumerate() {
        if !obs {
            continue;
        }
        let mut parts = vec![rect];
        for &(_, front) in stack[..i].iter().filter(|(o, _)| !o) {
            parts = parts.iter().flat_map(|p| subtract(p, &front)).collect();
        }
        out.extend(parts);
    }
    out
}

/// `a` less `b`: up to four rectangles.
fn subtract(a: &Rect, b: &Rect) -> Vec<Rect> {
    let (ax1, ay1, bx1, by1) = (a.x + a.width, a.y + a.height, b.x + b.width, b.y + b.height);
    if b.x >= ax1 || bx1 <= a.x || b.y >= ay1 || by1 <= a.y {
        return vec![*a];
    }
    let (top, bottom) = (b.y.max(a.y), by1.min(ay1));
    [
        Rect { x: a.x, y: a.y, width: a.width, height: top - a.y },
        Rect { x: a.x, y: bottom, width: a.width, height: ay1 - bottom },
        Rect { x: a.x, y: top, width: b.x.max(a.x) - a.x, height: bottom - top },
        Rect { x: bx1.min(ax1), y: top, width: ax1 - bx1.min(ax1), height: bottom - top },
    ]
    .into_iter()
    .filter(|r| r.width > 0.0 && r.height > 0.0)
    .collect()
}

/// Takes the real display or window as a PNG at `out`, without a sound or the pointer.
pub fn take(real: &Real, out: &Path) -> Result<()> {
    let program = std::env::var_os(CAPTURE_ENV).unwrap_or_else(|| "/usr/sbin/screencapture".into());
    let mut cmd = Command::new(&program);
    cmd.arg("-x");
    match real {
        Real::Display(d) => {
            let b = d.bounds;
            cmd.arg(format!("-R{},{},{},{}", b.x.round(), b.y.round(), b.width.round(), b.height.round()));
        }
        Real::Window(id) => {
            cmd.args(["-o", &format!("-l{id}")]);
        }
    }
    cmd.args(["-t", "png"]).arg(out).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let _ = std::fs::remove_file(out);
    let mut child = cmd.spawn().with_context(|| format!("starting {}", Path::new(&program).display()))?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > TAKE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("screencapture took over {} s", TAKE_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() || !out.exists() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            use std::io::Read;
            let _ = e.read_to_string(&mut err);
        }
        bail!("screencapture didn't take the {} ({status}): {}", real.describe(), err.trim());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn real(obs: Option<i32>) -> Result<Screen> {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use core_graphics::display::CGDisplay;
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowAlpha, kCGWindowBounds, kCGWindowLayer,
        kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly, kCGWindowOwnerPID,
    };
    use std::ffi::c_void;

    #[link(name = "ColorSync", kind = "framework")]
    extern "C" {
        fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFUUIDCreateString(alloc: *const c_void, uuid: *const c_void) -> CFStringRef;
        fn CFRelease(cf: *const c_void);
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    }

    // a command-line process needs a window-server connection before CoreGraphics' display calls
    // (CGS_REQUIRE_INIT); asking for the main display makes one
    let _ = CGDisplay::main();
    let ids =
        CGDisplay::active_displays().map_err(|e| anyhow::anyhow!("macOS didn't list its displays (CGError {e})"))?;
    let mut displays = Vec::new();
    for id in ids {
        let d = CGDisplay::new(id);
        let b = d.bounds();
        // SAFETY: returns a CFUUID we own (or null), released below; the string we own too
        let uuid = unsafe {
            let u = CGDisplayCreateUUIDFromDisplayID(id);
            if u.is_null() {
                String::new()
            } else {
                let s = CFUUIDCreateString(std::ptr::null(), u);
                CFRelease(u);
                if s.is_null() {
                    String::new()
                } else {
                    CFString::wrap_under_create_rule(s).to_string()
                }
            }
        };
        displays.push(Display {
            id,
            uuid,
            bounds: Rect { x: b.origin.x, y: b.origin.y, width: b.size.width, height: b.size.height },
            main: d.is_main(),
            asleep: d.is_asleep(),
        });
    }

    // SAFETY: a dictionary we own (or null), wrapped to be released
    let locked = unsafe {
        let s = CGSessionCopyCurrentDictionary();
        if s.is_null() {
            false
        } else {
            let info: CFDictionary<CFString, CFType> = CFDictionary::wrap_under_create_rule(s);
            info.find(CFString::new("CGSSessionScreenIsLocked"))
                .and_then(|v| v.downcast::<CFBoolean>())
                .is_some_and(bool::from)
        }
    };

    // the app windows front to back, OBS's or not: only the parts of OBS's that nothing covers are left out
    let mut stack = Vec::new();
    if let Some(pid) = obs {
        let options = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
        if let Some(list) = copy_window_info(options, kCGNullWindowID) {
            for item in list.iter() {
                // SAFETY: CGWindowListCopyWindowInfo's entries are CFDictionaries, which the array keeps alive
                let info: CFDictionary<CFString, CFType> =
                    unsafe { CFDictionary::wrap_under_get_rule(*item as CFDictionaryRef) };
                // SAFETY: CoreGraphics' own constant keys
                let get =
                    |key: CFStringRef| info.find(unsafe { CFString::wrap_under_get_rule(key) }).map(|v| v.clone());
                let number = |key| get(key).and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_f64());
                // SAFETY: as above
                let (layer, alpha, owner) = unsafe { (kCGWindowLayer, kCGWindowAlpha, kCGWindowOwnerPID) };
                if number(layer) != Some(0.0) || number(alpha).is_some_and(|a| a <= 0.0) {
                    continue; // the menu bar, the Dock, overlays, and windows that can't be seen
                }
                let Some(bounds) = get(unsafe { kCGWindowBounds }).and_then(|v| {
                    if v.type_of() != CFDictionary::<CFString, CFType>::type_id() {
                        return None;
                    }
                    // SAFETY: a CFDictionary, checked just now, which `v` keeps alive
                    let d: CFDictionary<CFString, CFType> =
                        unsafe { CFDictionary::wrap_under_get_rule(v.as_CFTypeRef() as CFDictionaryRef) };
                    let n = |k: &str| {
                        d.find(CFString::new(k)).and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_f64())
                    };
                    Some(Rect { x: n("X")?, y: n("Y")?, width: n("Width")?, height: n("Height")? })
                }) else {
                    continue;
                };
                stack.push((number(owner) == Some(pid as f64), bounds));
            }
        }
    }
    let obs_windows = visible(&stack);
    Ok(Screen { displays, obs_windows, locked })
}

#[cfg(not(target_os = "macos"))]
fn real(_obs: Option<i32>) -> Result<Screen> {
    bail!("the real screen is only read on macOS so far")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(main: bool, x: f64) -> Display {
        Display {
            id: if main { 1 } else { 2 },
            uuid: format!("UUID-{x}"),
            bounds: Rect { x, y: 0.0, width: 1000.0, height: 500.0 },
            main,
            asleep: false,
        }
    }

    #[test]
    fn a_display_is_found_by_uuid_number_place_or_as_the_main_one() {
        let screen = Screen { displays: vec![display(false, -1000.0), display(true, 0.0)], ..Screen::default() };
        assert_eq!(find(&screen, Some("uuid-0"), None, None).unwrap().id, 1, "any case");
        assert_eq!(find(&screen, None, Some(2), None).unwrap().id, 2);
        assert_eq!(find(&screen, None, None, Some(0)).unwrap().id, 2);
        assert_eq!(find(&screen, None, None, None).unwrap().id, 1);
        assert_eq!(find(&screen, Some("gone"), None, None), None, "a display that isn't connected");
    }

    #[test]
    fn a_covered_obs_window_hides_nothing_in_front_of_it() {
        let r = |x, y, width, height| Rect { x, y, width, height };
        // a browser over most of a full-screen OBS window, and a small window over the browser
        let stack = [
            (false, r(0.0, 0.0, 10.0, 10.0)),
            (false, r(0.0, 0.0, 800.0, 1000.0)),
            (true, r(0.0, 0.0, 1000.0, 1000.0)),
        ];
        let parts = visible(&stack);
        assert!(parts.iter().all(|p| p.x >= 800.0), "only the strip right of the browser: {parts:?}");
        assert_eq!(parts.iter().map(|p| p.width * p.height).sum::<f64>(), 200.0 * 1000.0);
        // OBS in front: all of it
        assert_eq!(
            visible(&[(true, r(0.0, 0.0, 100.0, 100.0)), (false, r(0.0, 0.0, 50.0, 50.0))]),
            vec![r(0.0, 0.0, 100.0, 100.0)]
        );
        // a hole in the middle leaves the four sides
        let parts = visible(&[(false, r(40.0, 40.0, 20.0, 20.0)), (true, r(0.0, 0.0, 100.0, 100.0))]);
        assert_eq!(parts.len(), 4);
        assert_eq!(parts.iter().map(|p| p.width * p.height).sum::<f64>(), 100.0 * 100.0 - 400.0);
    }

    #[test]
    fn obs_windows_mask_only_their_part_of_the_display() {
        let d = display(true, 0.0);
        let windows = [
            Rect { x: 500.0, y: 250.0, width: 1000.0, height: 1000.0 },
            Rect { x: 2000.0, y: 0.0, width: 10.0, height: 10.0 },
        ];
        assert_eq!(masks(&d, &windows), vec![Area { x: 0.5, y: 0.5, width: 0.5, height: 0.5 }]);
    }
}

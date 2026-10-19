//! Which sources to watch: the screen captures that are showing in OBS's program scene (in nested
//! scenes and groups too), or the one source the settings name, with what each shows on the real
//! screen, read from its settings.

use serde_json::{json, Value};

use crate::obs::{Obs, ObsError};

/// OBS's screen captures on macOS: the Screen Capture of OBS 28 and later (ScreenCaptureKit), and the
/// older Display Capture and Window Capture.
pub const CAPTURES: [&str; 3] = ["screen_capture", "display_capture", "window_capture"];

/// How deep nested scenes and groups are followed.
const DEPTH: usize = 4;

/// What a source shows on the real screen, by its settings.
#[derive(Debug, Clone, PartialEq)]
pub enum Shows {
    /// a display, by `display_uuid`, else its CoreGraphics number, else its place in the list; none of
    /// them: the main display
    Display {
        uuid: Option<String>,
        id: Option<u32>,
        index: Option<usize>,
    },
    Window(u32),
    /// nothing one picture of the screen matches (an application's windows), and why
    Nothing(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub name: String,
    pub kind: String,
    pub shows: Shows,
}

/// From a capture's kind and settings (`GetInputSettings`, which leaves out what's at its default).
pub fn shows(kind: &str, settings: &Value) -> Shows {
    let uuid = settings["display_uuid"].as_str().map(String::from);
    let number = settings["display"].as_u64();
    match kind {
        "screen_capture" => match settings["type"].as_u64().unwrap_or(0) {
            0 => Shows::Display { uuid, id: number.map(|n| n as u32), index: None },
            1 => match settings["window"].as_u64() {
                Some(w) => Shows::Window(w as u32),
                None => Shows::Nothing("a window capture with no window chosen".into()),
            },
            _ => Shows::Nothing("an application capture: no one window or display to compare it with".into()),
        },
        "display_capture" => Shows::Display { uuid, id: None, index: number.map(|n| n as usize) },
        "window_capture" => match settings["window"].as_u64() {
            Some(w) => Shows::Window(w as u32),
            None => Shows::Nothing("a window capture with no window chosen".into()),
        },
        // a source the settings name that isn't a capture: held against the main display
        _ => Shows::Display { uuid: None, id: None, index: None },
    }
}

/// The program scene's name, and the sources to watch in it: `wanted` alone if it's set, else every
/// screen capture showing there.
pub fn find(obs: &mut Obs, wanted: &str) -> Result<(String, Vec<Source>), ObsError> {
    let program = obs.request("GetCurrentProgramScene", json!({}))?;
    let scene = program["currentProgramSceneName"].as_str().or(program["sceneName"].as_str()).unwrap_or("").to_string();
    let mut found: Vec<(String, String)> = Vec::new();
    walk(obs, &scene, false, wanted, 0, &mut found)?;
    let mut sources = Vec::new();
    for (name, kind) in found {
        let settings = obs.request("GetInputSettings", json!({"inputName": name}))?;
        let kind = settings["inputKind"].as_str().map(String::from).unwrap_or(kind);
        sources.push(Source { shows: shows(&kind, &settings["inputSettings"]), name, kind });
    }
    Ok((scene, sources))
}

fn walk(
    obs: &mut Obs,
    scene: &str,
    group: bool,
    wanted: &str,
    depth: usize,
    found: &mut Vec<(String, String)>,
) -> Result<(), ObsError> {
    if depth > DEPTH {
        return Ok(());
    }
    let request = if group { "GetGroupSceneItemList" } else { "GetSceneItemList" };
    let items = obs.request(request, json!({"sceneName": scene}))?;
    for item in items["sceneItems"].as_array().into_iter().flatten() {
        if item["sceneItemEnabled"].as_bool() == Some(false) {
            continue;
        }
        let name = item["sourceName"].as_str().unwrap_or_default().to_string();
        let kind = item["inputKind"].as_str().unwrap_or_default().to_string();
        if item["isGroup"].as_bool() == Some(true) {
            walk(obs, &name, true, wanted, depth + 1, found)?;
        } else if item["sourceType"].as_str() == Some("OBS_SOURCE_TYPE_SCENE") {
            walk(obs, &name, false, wanted, depth + 1, found)?;
        } else if found.iter().all(|(n, _)| *n != name)
            && (if wanted.is_empty() { CAPTURES.contains(&kind.as_str()) } else { name == wanted })
        {
            found.push((name, kind));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_each_capture_shows() {
        let d = |uuid: Option<&str>, id, index| Shows::Display { uuid: uuid.map(String::from), id, index };
        assert_eq!(
            shows("screen_capture", &json!({})),
            d(None, None, None),
            "OBS leaves defaults out: the main display"
        );
        assert_eq!(shows("screen_capture", &json!({"display_uuid": "AB-CD"})), d(Some("AB-CD"), None, None));
        assert_eq!(shows("screen_capture", &json!({"display": 3})), d(None, Some(3), None));
        assert_eq!(shows("screen_capture", &json!({"type": 1, "window": 88})), Shows::Window(88));
        assert!(matches!(shows("screen_capture", &json!({"type": 2})), Shows::Nothing(_)));
        assert_eq!(shows("display_capture", &json!({"display": 1})), d(None, None, Some(1)));
        assert_eq!(shows("window_capture", &json!({"window": 5})), Shows::Window(5));
        assert_eq!(shows("image_source", &json!({"file": "x.png"})), d(None, None, None));
    }
}

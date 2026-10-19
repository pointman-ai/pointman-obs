//! The watchdog as a node runs it: the built binary under the SDK's stand-in node, against a fake OBS
//! and a fake screen (tests/common). Each looks every second and calls a picture frozen after 3, so a
//! freeze alerts within a few seconds.

mod common;

use std::time::Duration;

use common::*;
use serde_json::{json, Value};

const WITHIN: Duration = Duration::from_secs(15);

#[test]
fn a_frozen_capture_alerts_once_with_its_frame_then_all_clears_when_it_moves() {
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    let node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    // OBS's picture stays the same while the screen keeps moving: frozen
    let moving = Moving::start(&screen);

    let got = wait_posts(&posts, WITHIN, |p| !p.is_empty());
    assert_eq!(got.len(), 1, "one alert: {got:?}\n{}", node.log());
    let alert = &got[0];
    assert_eq!(alert["post"]["thread_id"], THREAD);
    let said = text(alert);
    assert!(said.starts_with("@james OBS's **Display Capture** has frozen on test-machine."), "{said}");
    assert!(said.contains("into the recording") && said.contains("hide and show **Display Capture**"), "{said}");
    assert_eq!(alert["asset"]["kind"], "image");
    assert_eq!(alert["png"], true, "the frozen frame goes with it: {alert}");
    let health = node.call("health.check", json!({})).unwrap();
    assert_eq!(part(&health, "watchdog")["state"], "failing", "{health}");
    assert!(part(&health, "watchdog")["why"].as_str().unwrap().starts_with("Display Capture frozen since"));

    // still frozen: nothing more
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(posts.lock().unwrap().len(), 1, "once per freeze");

    // its frames move again: the all-clear, on the same card
    fake.set(|s| {
        s.pictures.insert("Display Capture".into(), picture(640, 400, Some((300, 100, 40))));
    });
    let got = wait_posts(&posts, WITHIN, |p| p.len() >= 2);
    drop(moving);
    assert_eq!(got.len(), 2, "{got:?}");
    let said = text(&got[1]);
    assert!(said.starts_with("✅ OBS's **Display Capture** is moving again"), "{said}");
    assert!(!said.contains('@'), "the all-clear doesn't mention anyone again: {said}");
    assert_eq!(got[1]["asset"]["path"], got[0]["asset"]["path"], "the all-clear is the alert card's next version");
    let health = node.call("health.check", json!({})).unwrap();
    assert_eq!(part(&health, "watchdog")["state"], "ok", "{health}");
}

#[test]
fn a_freeze_whose_post_picture_fails_posts_the_looks_picture() {
    let mut state = ObsState::recording();
    state.fail = vec![("Display Capture".into(), 1280)];
    let (fake, screen, posts) = (FakeObs::start(state), FakeScreen::new(), Posts::default());
    let node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    let moving = Moving::start(&screen);
    let got = wait_posts(&posts, WITHIN, |p| !p.is_empty());
    drop(moving);
    assert_eq!(got.len(), 1, "the alert still goes: {}", node.log());
    assert!(text(&got[0]).starts_with("@james OBS's **Display Capture** has frozen"));
    assert_eq!(
        (got[0]["png"].clone(), got[0]["png_width"].clone()),
        (json!(true), json!(640)),
        "the look's own picture"
    );
    assert!(fake.state.lock().unwrap().fail.is_empty(), "the 1280 px one was asked for, and refused");
}

#[test]
fn a_stall_whose_picture_fails_alerts_at_the_next_look() {
    let mut state = ObsState::recording();
    state.fail = vec![("Main".into(), 1280)];
    let (fake, screen, posts) = (FakeObs::start(state), FakeScreen::new(), Posts::default());
    let node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    std::thread::sleep(Duration::from_secs(2));
    fake.set(|s| s.grow = false); // the file stops growing; the screen and the capture are still
    let got = wait_posts(&posts, WITHIN, |p| !p.is_empty());
    assert_eq!(got.len(), 1, "{}", node.log());
    assert!(
        text(&got[0]).starts_with("@james OBS's recording on test-machine hasn't grown since"),
        "{}",
        text(&got[0])
    );
    assert!(node.log().contains("trying again at the next look"), "the first try had no picture: {}", node.log());
    assert_eq!(got[0]["png_width"], 1280);
}

#[test]
fn no_mention_means_nobody_is_told_and_health_says_so() {
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    let node = obs(fake.port, &screen, &posts, json!({"mention": ""})).start().unwrap();
    let said = node.run("obs.status", json!({})).unwrap().result["text"].as_str().unwrap().to_string();
    assert!(said.contains("Alerts go to thread 77, mentioning nobody, so nobody is told"), "{said}");
    let health = node.call("health.check", json!({})).unwrap();
    assert_eq!(part(&health, "watchdog")["state"], "failing");
    assert!(part(&health, "watchdog")["why"].as_str().unwrap().starts_with("alerts mention nobody"), "{health}");
}

#[test]
fn a_still_screen_is_no_alarm() {
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    let node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    std::thread::sleep(Duration::from_secs(7));
    assert!(posts.lock().unwrap().is_empty(), "{:?}", posts.lock().unwrap());
    assert!(screen.calls().lines().count() >= 4, "the screen was looked at: {}", screen.calls());
    let health = node.call("health.check", json!({})).unwrap();
    assert_eq!(part(&health, "watchdog")["state"], "ok", "{health}");
    assert_eq!(part(&health, "watchdog")["why"], "watching Display Capture");
}

#[test]
fn a_capture_that_moves_with_the_screen_is_no_alarm() {
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    let _node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (flag, state) = (stop.clone(), fake.state.clone());
    let live = std::thread::spawn(move || {
        let mut n = 0;
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            state
                .lock()
                .unwrap()
                .pictures
                .insert("Display Capture".into(), picture(640, 400, Some(((n % 12) * 40, 100, 40))));
            n += 1;
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    let moving = Moving::start(&screen);
    std::thread::sleep(Duration::from_secs(7));
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    live.join().unwrap();
    drop(moving);
    assert!(posts.lock().unwrap().is_empty(), "{:?}", posts.lock().unwrap());
}

#[test]
fn obs_windows_moving_on_the_screen_are_no_alarm() {
    // OBS's own window (its meters, its timer) is the only thing moving, and the capture hides OBS
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    screen.list(json!({"obs_pid": 4242, "obs_windows": [{"x": 0, "y": 0, "width": 1512, "height": 500}]}));
    let _node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    let moving = Moving::start(&screen); // its square slides along y = 300..380, under OBS's window
    std::thread::sleep(Duration::from_secs(7));
    drop(moving);
    assert!(posts.lock().unwrap().is_empty(), "{:?}", posts.lock().unwrap());
}

#[test]
fn a_freeze_while_the_screen_is_locked_waits_for_it() {
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    screen.list(json!({"obs_pid": 4242, "locked": true}));
    let _node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    let moving = Moving::start(&screen);
    std::thread::sleep(Duration::from_secs(6));
    assert!(posts.lock().unwrap().is_empty(), "a locked screen isn't compared");
    assert!(screen.calls().is_empty(), "nor taken: {}", screen.calls());
    screen.list(json!({"obs_pid": 4242}));
    let got = wait_posts(&posts, WITHIN, |p| !p.is_empty());
    drop(moving);
    assert_eq!(got.len(), 1, "unlocked, the moving screen shows the freeze");
}

#[test]
fn the_recording_stopping_while_frozen_says_so_once() {
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    let _node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    let moving = Moving::start(&screen);
    wait_posts(&posts, WITHIN, |p| !p.is_empty());
    fake.set(|s| s.recording = false);
    let got = wait_posts(&posts, WITHIN, |p| p.len() >= 2);
    drop(moving);
    std::thread::sleep(Duration::from_secs(3));
    let got_after = posts.lock().unwrap().clone();
    assert_eq!(got_after.len(), 2, "{got:?}");
    assert!(
        text(&got[1]).starts_with("The recording stopped with **Display Capture** still frozen"),
        "{}",
        text(&got[1])
    );
}

#[test]
fn obs_closed_is_stopped_and_its_websocket_off_is_failing() {
    let (screen, posts) = (FakeScreen::new(), Posts::default());
    screen.list(json!({"obs_pid": null}));
    let port = closed_port();
    let node = obs(port, &screen, &posts, json!({})).start().unwrap();
    let done = node.run("obs.status", json!({})).unwrap();
    assert_eq!(done.result["text"].as_str().unwrap().lines().next(), Some("OBS isn't open on test-machine."));
    let health = node.call("health.check", json!({})).unwrap();
    assert_eq!(
        (part(&health, "obs")["state"].clone(), part(&health, "obs")["why"].clone()),
        (json!("stopped"), json!("OBS isn't open"))
    );
    assert_eq!(part(&health, "watchdog")["state"], "stopped");

    // OBS open, with its WebSocket server off
    screen.list(json!({"obs_pid": 4242}));
    let done = node.run("obs.status", json!({})).unwrap();
    let said = done.result["text"].as_str().unwrap();
    assert!(said.starts_with("OBS is open on test-machine, but nothing answers on its WebSocket port"), "{said}");
    let health = node.call("health.check", json!({})).unwrap();
    assert_eq!(part(&health, "obs")["state"], "failing");
    assert!(part(&health, "obs")["why"].as_str().unwrap().contains("Tools › WebSocket Server Settings"));
    assert!(posts.lock().unwrap().is_empty());
}

#[test]
fn a_wrong_password_needs_auth_and_the_right_one_connects() {
    let mut state = ObsState::recording();
    state.password = Some("right horse battery".into());
    let (fake, screen, posts) = (FakeObs::start(state), FakeScreen::new(), Posts::default());

    // none in the vault
    let node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    node.run("obs.status", json!({})).unwrap();
    let health = node.call("health.check", json!({})).unwrap();
    let p = part(&health, "obs");
    assert_eq!((p["state"].clone(), p["needs"].clone()), (json!("failing"), json!(["auth"])), "{health}");
    assert!(p["why"].as_str().unwrap().contains("asks for a password: add it with /obs add"), "{p}");
    node.stop();

    // a wrong one
    let node = obs(fake.port, &screen, &posts, json!({})).secret("obs.studio", "wrong").start().unwrap();
    let said = node.run("obs.status", json!({})).unwrap().result["text"].as_str().unwrap().to_string();
    assert!(said.contains("OBS refused the WebSocket password"), "{said}");
    let health = node.call("health.check", json!({})).unwrap();
    assert_eq!(part(&health, "obs")["needs"], json!(["auth"]));
    let fix = node.call("health.fix", json!({"part": "obs", "action": "auth"})).unwrap();
    assert!(fix["message"].as_str().unwrap().contains("/obs add"));
    node.stop();

    // a wrong one and the right one: the right one is used
    let node = obs(fake.port, &screen, &posts, json!({}))
        .secrets([("obs.old", "wrong"), ("obs.studio", "right horse battery")])
        .start()
        .unwrap();
    let said = node.run("obs.status", json!({})).unwrap().result["text"].as_str().unwrap().to_string();
    assert!(said.starts_with("OBS 32.2.2 on test-machine: recording"), "{said}");
    let health = node.call("health.check", json!({})).unwrap();
    assert_eq!(part(&health, "obs")["state"], "ok", "{health}");
}

#[test]
fn a_live_status_read_says_what_it_watches_and_where_alerts_go() {
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    let node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    let done = node.run("obs.status", json!({})).unwrap();
    let r = &done.result;
    let said = r["text"].as_str().unwrap();
    assert!(said.starts_with("OBS 32.2.2 on test-machine: recording, 00:00:"), "{said}");
    assert!(said.contains("in scene “Main”"), "{said}");
    assert!(said.contains("Watching **Display Capture** (display 1 (the main one))"), "{said}");
    assert!(said.ends_with("Alerts go to thread 77, mentioning @james."), "{said}");
    assert_eq!(r["watching"][0]["source"], "Display Capture");
    assert_eq!(r["recording"]["outputActive"], true);

    // not recording: nothing is watched
    fake.set(|s| s.recording = false);
    let said = node.run("obs.status", json!({})).unwrap().result["text"].as_str().unwrap().to_string();
    assert!(said.starts_with("OBS 32.2.2 on test-machine: not recording, so nothing is watched."), "{said}");
}

#[test]
fn nested_scenes_and_hidden_items_and_a_named_source() {
    let mut state = ObsState::recording();
    state.items = vec![
        json!({"sourceName": "Hidden Capture", "inputKind": "screen_capture", "isGroup": false,
               "sceneItemEnabled": false, "sourceType": "OBS_SOURCE_TYPE_INPUT"}),
        json!({"sourceName": "Frozen still", "inputKind": "image_source", "isGroup": false,
               "sceneItemEnabled": true, "sourceType": "OBS_SOURCE_TYPE_INPUT"}),
    ];
    state.inputs.insert("Frozen still".into(), ("image_source".into(), json!({"file": "/tmp/still.png"})));
    state.pictures.insert("Frozen still".into(), picture(640, 400, None));
    let (fake, screen, posts) = (FakeObs::start(state), FakeScreen::new(), Posts::default());

    // no capture showing: nothing watched
    let node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    let said = node.run("obs.status", json!({})).unwrap().result["text"].as_str().unwrap().to_string();
    assert!(said.contains("No screen capture is showing in the program scene"), "{said}");
    node.stop();

    // the settings name a source that isn't a capture (the live test's frozen stand-in): held against
    // the main display, and its freeze alerts
    let node = obs(fake.port, &screen, &posts, json!({"source": "Frozen still"})).start().unwrap();
    let moving = Moving::start(&screen);
    let got = wait_posts(&posts, WITHIN, |p| !p.is_empty());
    drop(moving);
    assert_eq!(got.len(), 1, "{}", node.log());
    assert!(text(&got[0]).starts_with("@james OBS's **Frozen still** has frozen"), "{}", text(&got[0]));
}

#[test]
fn no_thread_means_no_post_and_health_says_so() {
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    let node = obs(fake.port, &screen, &posts, json!({"thread": 0})).start().unwrap();
    node.run("obs.status", json!({})).unwrap();
    let health = node.call("health.check", json!({})).unwrap();
    assert_eq!(part(&health, "watchdog")["state"], "failing");
    assert!(part(&health, "watchdog")["why"].as_str().unwrap().starts_with("no thread to post alerts in"));
    let moving = Moving::start(&screen);
    std::thread::sleep(Duration::from_secs(6));
    drop(moving);
    assert!(posts.lock().unwrap().is_empty());
    assert!(node.log().contains("no thread set for alerts"), "{}", node.log());
}

#[test]
fn it_only_ever_reads_obs() {
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    let node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    let moving = Moving::start(&screen);
    wait_posts(&posts, WITHIN, |p| !p.is_empty());
    node.run("obs.status", json!({})).unwrap();
    drop(moving);
    let asked = fake.asked();
    assert!(asked.len() > 10, "{asked:?}");
    let writes: Vec<&String> = asked.iter().filter(|k| !k.starts_with("Get")).collect();
    assert!(writes.is_empty(), "only Get… requests: {writes:?}");
}

#[test]
fn an_alert_out_before_a_restart_isnt_sent_again() {
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    let state = tempfile::tempdir().unwrap();
    let node = obs(fake.port, &screen, &posts, json!({})).state_dir(state.path()).start().unwrap();
    let moving = Moving::start(&screen);
    wait_posts(&posts, WITHIN, |p| !p.is_empty());
    node.stop();
    let node = obs(fake.port, &screen, &posts, json!({})).state_dir(state.path()).start().unwrap();
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(posts.lock().unwrap().len(), 1, "still frozen after the restart: no second alert");
    fake.set(|s| {
        s.pictures.insert("Display Capture".into(), picture(640, 400, Some((300, 100, 40))));
    });
    let got = wait_posts(&posts, WITHIN, |p| p.len() >= 2);
    drop(moving);
    assert!(text(&got[1]).starts_with("✅"), "{}", text(&got[1]));
    drop(node);
}

#[test]
fn the_event_says_whats_watched_and_frozen() {
    let (fake, screen, posts) = (FakeObs::start(ObsState::recording()), FakeScreen::new(), Posts::default());
    let node = obs(fake.port, &screen, &posts, json!({})).start().unwrap();
    let moving = Moving::start(&screen);
    let got =
        node.wait_event_where("obs.status", WITHIN, |e: &Value| e["frozen"] == json!(["Display Capture"])).unwrap();
    drop(moving);
    assert_eq!((got["recording"].clone(), got["watching"].clone()), (json!(true), json!(["Display Capture"])));
}

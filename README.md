# obs

A Pointman extension for OBS: a recording watchdog. While OBS records, it holds each screen capture's
picture against the real screen, and when the picture stops while the screen moves, it tells you in a
thread within about a minute, with the frozen frame. Then an all-clear when its frames move again.

Why: while James recorded Spacesheep, OBS's display capture froze at 11:14 and kept writing that one
frame at 60 fps for the next 1 h 20 min, with no warning: 31 of its 36 minutes of screen footage were
lost (Pointman thread 10399). A frozen capture still writes a normal-looking file, only a small one.

One Rust binary, `bin/obs`, on the [pointman-extension](https://github.com/pointman-ai/pointman-extension)
crate (the node's typed protocol: each method's params and result are its messages), for macOS (Apple silicon
and Intel).

## How it tells

- **Reading OBS:** obs-websocket v5, built into OBS 28 and later, on `127.0.0.1:4455`. It sends only
  `Get…` requests (`GetRecordStatus`, `GetCurrentProgramScene`, `GetSceneItemList`, `GetInputSettings`,
  `GetSourceScreenshot`, `GetVersion`): `Obs::request` refuses anything else, so it can't change a scene,
  a source, a profile or the recording. It subscribes to no events.
- **What it watches:** every macOS Screen Capture (and the older Display Capture and Window Capture)
  showing in the program scene, through nested scenes and groups, or the one source the `source`
  setting names. Hidden items aren't watched.
- **A look,** every `every_seconds` (15) while recording and not paused:
  1. the real screen, with macOS's `screencapture`: the capture's display (by its `display_uuid`) or
     window (by its id), then
  2. OBS's picture of the source (`GetSourceScreenshot`, 640 px across).
  Each is reduced to a 256-cell-wide grid of colour averages and compared with the last look's. The
  screen goes first, so a change that lands between the two shows in OBS's and is never taken for a freeze.
- **Frozen:** OBS's picture hasn't changed at all (no cell by more than 4 of 255) for `frozen_seconds`
  (60), while the screen has (0.05% of its cells by more than 12), seen at a look before the latest. A
  frozen capture is the same texture each time, so its picture doesn't change by a single cell, while a
  live one shows any move of the screen in many cells.
- **Not frozen:** a still screen (neither changes), a capture that moves with the screen, OBS's own
  windows (the parts of them nothing covers are left out of the screen's comparison: their meters and
  timer move all the time, and a capture that hides OBS never shows them), a locked or sleeping screen (not taken at all), and an application capture (no one
  picture to compare it with: listed as such).
- **The file:** `GetRecordStatus`'s `outputBytes`. The alert says how fast it grew over the last minute
  (a frozen 60 fps capture wrote 153 MB in 20 min, 0.13 MB/s). A recording that has grown and then
  doesn't grow for `frozen_seconds` is an alert of its own ("OBS may have stopped writing it").

## What it posts

In the `thread` the settings name, as a `files.publish` of OBS's picture with a post:

| When | Post |
|---|---|
| A capture freezes | `@james OBS's **Display Capture** has frozen on mac-m4. Its picture hasn't changed since 11:14:05 (00:12:34 into the recording) while the screen has…`, with how to bring it back (hide and show the source, or pick its display again; then stop and start the recording), and the frozen frame |
| Its frames move again | `✅ … is moving again (11:20:40). It was frozen for 6 min: 00:12:34–00:19:09 of the recording shows one frame.` The same card's next version, with the moving frame. No mention |
| The recording stops, or OBS closes, while frozen | `The recording stopped with **Display Capture** still frozen: from 00:12:34 to its end, it shows one frame.` |
| The frozen source leaves the program scene | That it's no longer watched, and what part of the recording is frozen |
| The file stops growing, and grows again | An alert with `mention`, then `✅` |

The picture is OBS's at 1280 px across; if OBS doesn't give one just then, the look's own (640 px) goes
with the alert. A stall's picture is the program scene's, and with none, it's tried again at the next look.

Once per freeze: an alert that's out is kept in the state folder (`alerts.json`), so a restart doesn't
send it again, and the next move is its all-clear.

**Not a Needs-you card yet:** an extension can't raise one through the node today (only shells and
agents' hooks do), so the alert notifies by its `@mention`. A node method for it is patchbay's, later.

## Setting it up

1. **OBS:** Tools › WebSocket Server Settings › Enable WebSocket server. Keep authentication on.
2. **The password:** `/obs add` in any thread opens the app's form, which seals it in the vault as
   `obs.<label>` (OBS's Show Connect Info has it). The extension tries each `obs.` item until OBS takes one.
   It never reads OBS's config files.
3. **Screen Recording:** the node needs it (System Settings › Privacy & Security › Screen & System Audio
   Recording), as for the screen extension. Without it, `screencapture` takes only the desktop, so a
   freeze over windows goes unseen.
4. **Settings,** in `~/.config/pointman/extensions/obs/config.toml` (or `pointman config set
   node.extensions.obs.<key> <value>`):

| Setting | Default | |
|---|---|---|
| `thread` | none | The thread alerts go to. Without one nothing is posted, and health says so |
| `mention` | none | Who an alert @mentions (`@james`) |
| `every_seconds` | 15 | How often it looks while recording |
| `frozen_seconds` | 60 | How long a picture may stay the same while the screen moves |
| `source` | every screen capture | One source by name; one that isn't a capture is held against the main display |
| `host`, `port` | `127.0.0.1`, `4455` | OBS's WebSocket server |

With the defaults a freeze alerts 60–75 s after OBS's picture last moved: the 60 s rule, plus up to
one look.

## Commands and health

| Kind | Slash | What |
|---|---|---|
| `obs.status` | `/obs status` | A live read: OBS open or not, its WebSocket, the recording (timecode, size), the program scene, each watched source and its state, and where alerts go |
| `obs.add` | `/obs add` | The app's form for the WebSocket password |

Health has two parts. `obs`: OBS isn't open (stopped), open with its WebSocket server off, asking for a
password (needs auth: `/obs add`), refusing the vault's (needs auth), or ok with the recording's state.
`watchdog`: failing while a capture is frozen or the file has stalled, with no `thread` set, or with no
`mention` (an alert nobody is told about); else what it watches. The `obs.status` event says the same when it changes.

## Development

`cargo test`. The tests run obs under the SDK's stand-in node against a fake OBS (an obs-websocket v5
server in the test, with a password or not, answering from a state the test changes) and a fake screen
(`POINTMAN_OBS_FAKE`: the displays, OBS's windows and whether it's locked; `POINTMAN_OBS_SCREENCAPTURE`:
a screencapture that copies the picture the test puts in place), so no test reads this machine's OBS or
screen. They look every second and call a picture frozen after 3: a freeze alerts once with its frame and
all-clears on the same card, a still screen and a live capture don't, OBS's own windows moving don't,
a locked screen waits, the recording stopping says so once, OBS closed, its WebSocket off, no password
and a wrong one, a restart mid-freeze, and that only `Get…` requests are ever sent. `src/watch.rs` has the
judgement's own tests, on looks alone.

`tests/real_screen.rs` is by hand only (`cargo test --release --test real_screen -- --ignored --nocapture`):
it takes the real main display twice and prints sizes, times and how much changed, then deletes them.

A release is a tag that's extension.toml's version: .github/workflows/release.yml builds both Macs and
attaches them.

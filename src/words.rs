//! What the watchdog posts and says: the alert, its all-clear and the rest, short enough for a phone.

use crate::watch::Mark;

fn local(at: f64) -> Option<libc::tm> {
    let t = at as libc::time_t;
    // SAFETY: a zeroed tm is a valid one, and localtime_r only writes the tm it's given
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    (!unsafe { libc::localtime_r(&t, &mut tm) }.is_null()).then_some(tm)
}

/// A time of day on this machine, `11:14:05`.
pub fn clock(at: f64) -> String {
    match local(at) {
        Some(tm) => format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
        None => format!("{at:.0}"),
    }
}

/// A moment as part of a file's name, `20261020-111405`.
pub fn stamp(at: f64) -> String {
    match local(at) {
        Some(tm) => format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ),
        None => format!("{at:.0}"),
    }
}

/// A name as part of a path: `display-capture`.
pub fn slug(name: &str) -> String {
    let s: String = name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let s = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    if s.is_empty() {
        "source".into()
    } else {
        s
    }
}

/// A span of time, as people say it: `45 s`, `6 min`, `1 h 20 min`.
pub fn span(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    match s {
        0..=89 => format!("{s} s"),
        90..=3599 => format!("{} min", (s + 30) / 60),
        _ => {
            let m = (s + 30) / 60;
            match m % 60 {
                0 => format!("{} h", m / 60),
                rest => format!("{} h {rest} min", m / 60),
            }
        }
    }
}

/// A size, `153 MB`, `1.2 GB`.
pub fn size(bytes: u64) -> String {
    let mb = bytes as f64 / 1e6;
    if mb >= 1000.0 {
        format!("{:.1} GB", mb / 1000.0)
    } else {
        format!("{mb:.0} MB")
    }
}

/// A recording's timecode without its milliseconds: `00:12:34`.
pub fn timecode(tc: &str) -> &str {
    tc.split('.').next().unwrap_or(tc)
}

fn lead(mention: &str) -> String {
    match mention.trim() {
        "" => String::new(),
        m if m.starts_with('@') => format!("{m} "),
        m => format!("@{m} "),
    }
}

/// What a capture shows, for "pick the display again".
pub fn what(real: &str) -> &'static str {
    if real.starts_with("window") {
        "window"
    } else {
        "display"
    }
}

/// The alert: which source froze, since when, and how to bring it back.
pub fn frozen(mention: &str, machine: &str, source: &str, shows: &str, since: &Mark, rate: Option<f64>) -> String {
    let rate =
        rate.map(|r| format!(", and the file grew {:.2} MB/s over the last minute", r / 1e6)).unwrap_or_default();
    format!(
        "{}OBS's **{source}** has frozen on {machine}. Its picture hasn't changed since {} ({} into the recording) \
         while the screen has, so the recording is writing that one frame{rate}.\n\n\
         **To bring it back:** in OBS's Sources, hide and show **{source}** (its eye), or open its Properties and pick \
         the {} again. If it stays frozen, stop and start the recording. An all-clear follows here when its frames move.",
        lead(mention),
        clock(since.at),
        timecode(&since.timecode),
        what(shows),
    )
}

/// The all-clear.
pub fn moving(source: &str, since: &Mark, at: &Mark) -> String {
    format!(
        "✅ OBS's **{source}** is moving again ({}). It was frozen for {}: {}–{} of the recording shows one frame.",
        clock(at.at),
        span(at.at - since.at),
        timecode(&since.timecode),
        timecode(&at.timecode),
    )
}

/// The recording ended (or OBS closed) with the capture still frozen.
pub fn ended(source: &str, since: &Mark, how: &str) -> String {
    format!(
        "The recording {how} with **{source}** still frozen: from {} to its end, it shows one frame.",
        timecode(&since.timecode),
    )
}

/// The frozen source isn't in the program scene any more.
pub fn gone(source: &str, scene: &str, since: &Mark, at: &Mark) -> String {
    format!(
        "**{source}** isn't in the program scene any more (it's “{scene}” now), so it's no longer watched. It was \
         frozen from {} to {} of the recording.",
        timecode(&since.timecode),
        timecode(&at.timecode),
    )
}

/// The recording file stopped growing.
pub fn stalled(mention: &str, machine: &str, since: &Mark, bytes: u64) -> String {
    format!(
        "{}OBS's recording on {machine} hasn't grown since {} ({} into it): it's still {}. OBS may have stopped \
         writing it: check the disk has room, and OBS's status bar.",
        lead(mention),
        clock(since.at),
        timecode(&since.timecode),
        size(bytes),
    )
}

/// It grows again.
pub fn growing(since: &Mark, at: &Mark) -> String {
    format!("✅ OBS's recording is growing again ({}), after {} without.", clock(at.at), span(at.at - since.at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_and_sizes_read_as_people_say_them() {
        assert_eq!(span(45.0), "45 s");
        assert_eq!(span(370.0), "6 min");
        assert_eq!(span(4800.0), "1 h 20 min");
        assert_eq!(span(7200.0), "2 h");
        assert_eq!(size(153_000_000), "153 MB");
        assert_eq!(size(1_234_000_000), "1.2 GB");
        assert_eq!(timecode("00:12:34.567"), "00:12:34");
    }

    #[test]
    fn the_alert_mentions_once_and_says_how_to_bring_it_back() {
        let since = Mark { at: 0.0, timecode: "00:12:34.500".into() };
        let text = frozen("james", "mac-m4", "Display Capture", "display 1", &since, Some(130_000.0));
        assert!(text.starts_with("@james OBS's **Display Capture** has frozen on mac-m4."), "{text}");
        assert!(text.contains("(00:12:34 into the recording)"), "{text}");
        assert!(text.contains("grew 0.13 MB/s"), "{text}");
        assert!(text.contains("pick the display again"), "{text}");
        assert!(!frozen("", "m", "s", "window 7", &since, None).starts_with('@'));
    }
}

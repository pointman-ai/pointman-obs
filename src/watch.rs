//! The watchdog's judgement, from looks alone (no OBS, no clock, so the tests drive it directly).
//!
//! - **Frozen:** a capture's picture hasn't changed for `frozen` seconds while the screen it shows has.
//!   The screen's move has to be seen at an earlier look than the latest still picture, so a change
//!   that lands between taking the screen and taking OBS's picture is never a freeze.
//! - **Still:** neither changed. Nothing to say: a still screen is no alarm.
//! - **Once per freeze:** after the alert, nothing until the picture moves again (the all-clear).
//! - **The file:** a recording that has grown and then stops growing for `frozen` seconds is a stall
//!   of its own, with its own all-clear.

use std::collections::VecDeque;

/// When something was seen: the time (Unix seconds) and the recording's timecode then.
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub at: f64,
    pub timecode: String,
}

/// One look at one source: whether OBS's picture and the real screen changed since the last look
/// (None: nothing to compare with, the first look, or a screen that couldn't be taken).
#[derive(Debug, Clone)]
pub struct Look {
    pub at: Mark,
    pub picture_moved: Option<bool>,
    pub screen_moved: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// its picture has been the same since `since`, while the screen moved
    Frozen { since: Mark },
    /// it moved again at `at`, after the freeze alerted from `since`
    Moving { since: Mark, at: Mark },
}

/// One source's state between looks.
#[derive(Debug, Clone, Default)]
pub struct Watch {
    /// the first look of the current run of unchanged pictures
    pub still_since: Option<Mark>,
    /// when the screen was first seen to move during that run
    pub screen_moved_at: Option<f64>,
    /// the freeze alerted, from when
    pub alerted: Option<Mark>,
}

impl Watch {
    /// A watch with its alert already out (carried over a restart): the next move is its all-clear.
    pub fn alerted(since: Mark) -> Watch {
        Watch { alerted: Some(since), ..Watch::default() }
    }

    pub fn look(&mut self, look: &Look, frozen: f64) -> Option<Verdict> {
        match look.picture_moved {
            None | Some(true) => {
                self.still_since = Some(look.at.clone());
                self.screen_moved_at = None;
                if look.picture_moved == Some(true) {
                    if let Some(since) = self.alerted.take() {
                        return Some(Verdict::Moving { since, at: look.at.clone() });
                    }
                }
                None
            }
            Some(false) => {
                if self.alerted.is_some() {
                    return None;
                }
                let since = self.still_since.get_or_insert_with(|| look.at.clone()).clone();
                let earlier = self.screen_moved_at.is_some_and(|t| t < look.at.at);
                if look.screen_moved == Some(true) && self.screen_moved_at.is_none() {
                    self.screen_moved_at = Some(look.at.at);
                }
                if earlier && look.at.at - since.at >= frozen {
                    self.alerted = Some(since.clone());
                    return Some(Verdict::Frozen { since });
                }
                None
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stall {
    /// the file hasn't grown since `since` (`bytes` then)
    Stalled { since: Mark, bytes: u64 },
    /// it grew again at `at`
    Growing { since: Mark, at: Mark },
}

/// The recording file's size over the last minute or so: its rate, and whether it stopped growing.
#[derive(Debug, Clone, Default)]
pub struct Growth {
    seen: VecDeque<(f64, u64)>,
    still_since: Option<Mark>,
    pub alerted: Option<Mark>,
}

/// How far back the rate looks.
const RATE_OVER: f64 = 60.0;

impl Growth {
    pub fn look(&mut self, at: &Mark, bytes: u64, frozen: f64) -> Option<Stall> {
        let grew = self.seen.back().is_some_and(|&(_, last)| bytes > last);
        let started = self.seen.back().is_none_or(|&(_, last)| bytes < last);
        if started {
            // a new recording (or the first look): nothing to say about the last one's
            self.seen.clear();
            self.still_since = None;
            self.alerted = None;
        }
        self.seen.push_back((at.at, bytes));
        while self.seen.len() > 2 && self.seen.front().is_some_and(|&(t, _)| at.at - t > RATE_OVER) {
            self.seen.pop_front();
        }
        if grew || started {
            self.still_since = Some(at.clone());
            if grew {
                if let Some(since) = self.alerted.take() {
                    return Some(Stall::Growing { since, at: at.clone() });
                }
            }
            return None;
        }
        // only a file that has grown can stall: OBS gives 0 for an output that doesn't count its bytes
        if bytes == 0 || self.alerted.is_some() {
            return None;
        }
        let since = self.still_since.get_or_insert_with(|| at.clone()).clone();
        if at.at - since.at >= frozen {
            self.alerted = Some(since.clone());
            return Some(Stall::Stalled { since, bytes });
        }
        None
    }

    /// Bytes a second over the looks of the last minute, once there are two.
    pub fn rate(&self) -> Option<f64> {
        let (&(t0, b0), &(t1, b1)) = (self.seen.front()?, self.seen.back()?);
        (t1 > t0).then(|| b1.saturating_sub(b0) as f64 / (t1 - t0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(at: f64) -> Mark {
        Mark { at, timecode: format!("00:00:{at:02.0}") }
    }

    /// Looks every `every` seconds from 0: (picture moved, screen moved) for each.
    fn run(watch: &mut Watch, every: f64, looks: &[(Option<bool>, Option<bool>)]) -> Vec<(f64, Verdict)> {
        let mut out = Vec::new();
        for (i, &(picture_moved, screen_moved)) in looks.iter().enumerate() {
            let at = mark(i as f64 * every);
            if let Some(v) = watch.look(&Look { at: at.clone(), picture_moved, screen_moved }, 60.0) {
                out.push((at.at, v));
            }
        }
        out
    }

    const FIRST: (Option<bool>, Option<bool>) = (None, None);
    const STILL: (Option<bool>, Option<bool>) = (Some(false), Some(false));
    const SCREEN_ONLY: (Option<bool>, Option<bool>) = (Some(false), Some(true));
    const BOTH: (Option<bool>, Option<bool>) = (Some(true), Some(true));

    #[test]
    fn a_frozen_picture_while_the_screen_moves_alerts_once_after_a_minute() {
        let mut w = Watch::default();
        // moving, then frozen from 30 s while the screen keeps moving
        let looks = [FIRST, BOTH, BOTH, SCREEN_ONLY, SCREEN_ONLY, SCREEN_ONLY, SCREEN_ONLY, SCREEN_ONLY, SCREEN_ONLY];
        let got = run(&mut w, 15.0, &looks);
        assert_eq!(got, vec![(90.0, Verdict::Frozen { since: mark(30.0) })], "60 s after its last move, once");
    }

    #[test]
    fn a_still_screen_is_no_alarm() {
        let mut w = Watch::default();
        let looks = [FIRST, BOTH, STILL, STILL, STILL, STILL, STILL, STILL, STILL, STILL, STILL, STILL];
        assert_eq!(run(&mut w, 15.0, &looks), vec![]);
    }

    #[test]
    fn a_long_still_screen_then_a_move_alerts_on_the_next_look() {
        let mut w = Watch::default();
        let mut looks = vec![FIRST, BOTH];
        looks.extend([STILL; 10]); // 150 s of a still screen, the picture still too
        looks.push(SCREEN_ONLY); // the screen moves at 180 s: is OBS's picture behind it, or frozen?
        looks.push(STILL); // 195 s: still the same picture, so frozen
        let got = run(&mut w, 15.0, &looks);
        assert_eq!(got, vec![(195.0, Verdict::Frozen { since: mark(15.0) })]);
    }

    #[test]
    fn a_picture_that_catches_up_on_the_next_look_is_no_alarm() {
        let mut w = Watch::default();
        let mut looks = vec![FIRST, BOTH];
        looks.extend([STILL; 10]);
        looks.push(SCREEN_ONLY); // the move landed between taking the screen and OBS's picture
        looks.push(BOTH); // and OBS has it at the next look
        assert_eq!(run(&mut w, 15.0, &looks), vec![]);
    }

    #[test]
    fn recovery_is_one_all_clear_then_it_watches_afresh() {
        let mut w = Watch::default();
        let mut looks = vec![FIRST, BOTH];
        looks.extend([SCREEN_ONLY; 6]); // frozen from 15 s, alerted at 75 s
        looks.push(BOTH); // moving again at 120 s
        looks.extend([STILL; 6]); // still, no alarm
        let got = run(&mut w, 15.0, &looks);
        assert_eq!(
            got,
            vec![
                (75.0, Verdict::Frozen { since: mark(15.0) }),
                (120.0, Verdict::Moving { since: mark(15.0), at: mark(120.0) }),
            ]
        );
        assert!(w.alerted.is_none());
    }

    #[test]
    fn an_alert_carried_over_a_restart_clears_on_the_first_move() {
        let mut w = Watch::alerted(mark(5.0));
        let got = run(&mut w, 15.0, &[FIRST, SCREEN_ONLY, SCREEN_ONLY, BOTH]);
        assert_eq!(got, vec![(45.0, Verdict::Moving { since: mark(5.0), at: mark(45.0) })]);
    }

    #[test]
    fn a_screen_that_cant_be_taken_is_no_evidence() {
        let mut w = Watch::default();
        let mut looks = vec![FIRST, BOTH];
        looks.extend([(Some(false), None); 10]); // locked, asleep, or no screen to compare
        assert_eq!(run(&mut w, 15.0, &looks), vec![]);
    }

    #[test]
    fn a_file_that_stops_growing_stalls_once_then_grows_again() {
        let mut g = Growth::default();
        let sizes = [0, 1000, 2000, 3000, 3000, 3000, 3000, 3000, 3000, 4000];
        let got: Vec<_> =
            sizes.iter().enumerate().filter_map(|(i, &b)| g.look(&mark(i as f64 * 15.0), b, 60.0)).collect();
        assert_eq!(
            got,
            vec![
                Stall::Stalled { since: mark(45.0), bytes: 3000 },
                Stall::Growing { since: mark(45.0), at: mark(135.0) },
            ]
        );
    }

    #[test]
    fn a_file_whose_size_obs_doesnt_count_never_stalls() {
        let mut g = Growth::default();
        assert!((0..10).all(|i| g.look(&mark(i as f64 * 15.0), 0, 60.0).is_none()));
    }

    #[test]
    fn the_rate_is_over_the_last_minute() {
        let mut g = Growth::default();
        for i in 0..10 {
            g.look(&mark(i as f64 * 15.0), i * 1_500_000, 60.0);
        }
        assert_eq!(g.rate(), Some(100_000.0));
    }
}

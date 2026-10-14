//! Switching state machine: dwell, minimum interval, manual override.
//!
//! Pure and clock-free: every event carries its own `Instant`, so tests drive
//! it with synthetic time. Rules:
//!
//! - An automatic switch needs the *opposing* camera to be selected,
//!   uncontradicted, for `dwell` (800 ms by default), with at least
//!   `min_votes` opposing verdicts, and the deciding tick itself must be an
//!   opposing verdict.
//! - Contrary evidence restarts the dwell immediately: the current camera
//!   winning, ambiguous scores, both cameras non-frontal, looking down.
//! - Missing evidence (face tracking dropped out on one or both cameras) never
//!   switches, but a gap of up to `max_gap` (500 ms) does not discard the
//!   dwell. Longer gaps do. `max_gap = 0` gives strict every-tick behaviour.
//! - Two automatic switches are at least `min_interval` apart (1500 ms).
//!   A dwell that completes early waits, and still has to hold.
//! - Manual selection switches immediately and enters Manual mode, where the
//!   tracker is ignored until the user returns to Auto.
//! - A scene change made in OBS by hand is adopted as the current camera and
//!   restarts both timers, so AutoCam does not fight the user.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::CamId;
use crate::scoring::Decision;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Auto,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchParams {
    pub dwell: Duration,
    pub min_interval: Duration,
    /// Longest tolerated run of missing evidence inside a dwell.
    pub max_gap: Duration,
    /// Minimum opposing verdicts within a dwell.
    pub min_votes: u32,
}

impl Default for SwitchParams {
    fn default() -> Self {
        Self {
            dwell: Duration::from_millis(800),
            min_interval: Duration::from_millis(1500),
            max_gap: Duration::from_millis(500),
            min_votes: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    cam: CamId,
    since: Instant,
    votes: u32,
    /// Start of the current run of missing-evidence ticks, if any.
    gap_since: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// One analysis tick's verdict.
    Tick {
        now: Instant,
        decision: Decision,
    },
    /// User picked a camera (hotkey, console, HTTP). Enters Manual mode.
    ManualSelect {
        now: Instant,
        cam: CamId,
    },
    SetMode {
        now: Instant,
        mode: Mode,
    },
    /// OBS reports the program scene (on connect, or changed by hand).
    /// `None` if Program shows a scene that belongs to neither camera.
    ProgramScene {
        now: Instant,
        cam: Option<CamId>,
    },
    /// The last emitted switch could not be applied.
    SwitchFailed {
        now: Instant,
        previous: Option<CamId>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchCause {
    Auto,
    Manual,
}

/// An instruction for the OBS transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchCommand {
    pub to: CamId,
    pub cause: SwitchCause,
    /// For auto switches: when the opposing camera first started winning.
    /// `now - since` is the decision latency.
    pub since: Instant,
}

#[derive(Debug, Clone)]
pub struct Switcher {
    params: SwitchParams,
    mode: Mode,
    current: Option<CamId>,
    /// Opposing camera currently accumulating dwell.
    pending: Option<Pending>,
    last_switch: Option<Instant>,
}

impl Switcher {
    pub fn new(params: SwitchParams, mode: Mode) -> Self {
        Self {
            params,
            mode,
            current: None,
            pending: None,
            last_switch: None,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn current(&self) -> Option<CamId> {
        self.current
    }

    /// Camera accumulating dwell, and since when.
    pub fn pending(&self) -> Option<(CamId, Instant)> {
        self.pending.map(|p| (p.cam, p.since))
    }

    pub fn handle(&mut self, event: Event) -> Option<SwitchCommand> {
        match event {
            Event::Tick { now, decision } => self.tick(now, decision),
            Event::ManualSelect { now, cam } => {
                self.mode = Mode::Manual;
                self.pending = None;
                self.last_switch = Some(now);
                // Always emit, even if already current: the user may have
                // changed Program in OBS a moment ago and expects this to win.
                self.current = Some(cam);
                Some(SwitchCommand {
                    to: cam,
                    cause: SwitchCause::Manual,
                    since: now,
                })
            }
            Event::SetMode { now: _, mode } => {
                self.mode = mode;
                self.pending = None;
                None
            }
            Event::ProgramScene { now, cam } => {
                if cam != self.current {
                    self.current = cam;
                    self.pending = None;
                    self.last_switch = Some(now);
                }
                None
            }
            Event::SwitchFailed { now: _, previous } => {
                self.current = previous;
                self.pending = None;
                None
            }
        }
    }

    fn tick(&mut self, now: Instant, decision: Decision) -> Option<SwitchCommand> {
        if self.mode == Mode::Manual {
            self.pending = None;
            return None;
        }
        let cam = match decision {
            Decision::Prefer { cam, .. } if Some(cam) != self.current => cam,
            Decision::Hold { reason } if reason.is_neutral() => {
                // Missing evidence: keep the current camera; keep the dwell
                // only while the gap stays shorter than `max_gap`.
                if let Some(p) = self.pending.as_mut() {
                    let gap_since = *p.gap_since.get_or_insert(now);
                    if now.saturating_duration_since(gap_since) >= self.params.max_gap {
                        self.pending = None;
                    }
                }
                return None;
            }
            // Contrary evidence (current camera wins, ambiguous, low score,
            // looking down): restart the dwell.
            _ => {
                self.pending = None;
                return None;
            }
        };
        let p = match self.pending {
            Some(p) if p.cam == cam => Pending {
                votes: p.votes + 1,
                gap_since: None,
                ..p
            },
            _ => Pending {
                cam,
                since: now,
                votes: 1,
                gap_since: None,
            },
        };
        self.pending = Some(p);
        let since = p.since;
        let dwell_ok = now.saturating_duration_since(since) >= self.params.dwell
            && p.votes >= self.params.min_votes;
        let interval_ok = self
            .last_switch
            .is_none_or(|t| now.saturating_duration_since(t) >= self.params.min_interval);
        if dwell_ok && interval_ok {
            self.current = Some(cam);
            self.pending = None;
            self.last_switch = Some(now);
            Some(SwitchCommand {
                to: cam,
                cause: SwitchCause::Auto,
                since,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scoring::HoldReason;

    const TICK: Duration = Duration::from_millis(250);

    fn prefer(cam: CamId) -> Decision {
        Decision::Prefer {
            cam,
            score: 0.9,
            margin: 0.8,
        }
    }

    fn hold(reason: HoldReason) -> Decision {
        Decision::Hold { reason }
    }

    /// Drives ticks at 4 Hz; returns (tick index, command) for every switch.
    struct Rig {
        sw: Switcher,
        t0: Instant,
        i: u32,
    }

    impl Rig {
        fn new(current: CamId) -> Self {
            let t0 = Instant::now();
            let mut sw = Switcher::new(SwitchParams::default(), Mode::Auto);
            sw.handle(Event::ProgramScene {
                now: t0,
                cam: Some(current),
            });
            // Make the startup adoption old enough not to block the first switch.
            Self {
                sw,
                t0: t0 + Duration::from_secs(10),
                i: 0,
            }
        }

        fn now(&self) -> Instant {
            self.t0 + TICK * self.i
        }

        fn tick(&mut self, d: Decision) -> Option<SwitchCommand> {
            self.i += 1;
            let now = self.now();
            self.sw.handle(Event::Tick { now, decision: d })
        }

        fn run(&mut self, d: Decision, n: u32) -> Vec<(u32, SwitchCommand)> {
            (0..n)
                .filter_map(|_| self.tick(d).map(|c| (self.i, c)))
                .collect()
        }
    }

    #[test]
    fn switches_after_dwell_not_before() {
        let mut r = Rig::new(CamId::Left);
        // Ticks at 250, 500, 750 ms after the first opposing verdict: no switch.
        assert!(r.tick(prefer(CamId::Right)).is_none()); // dwell starts here
        assert!(r.tick(prefer(CamId::Right)).is_none());
        assert!(r.tick(prefer(CamId::Right)).is_none());
        assert!(r.tick(prefer(CamId::Right)).is_none()); // 750 ms
        let cmd = r
            .tick(prefer(CamId::Right))
            .expect("switch at 1000 ms >= 800");
        assert_eq!(cmd.to, CamId::Right);
        assert_eq!(cmd.cause, SwitchCause::Auto);
        assert_eq!(r.now() - cmd.since, Duration::from_millis(1000));
        assert_eq!(r.sw.current(), Some(CamId::Right));
    }

    #[test]
    fn exact_dwell_boundary() {
        let t0 = Instant::now();
        let mut sw = Switcher::new(SwitchParams::default(), Mode::Auto);
        sw.handle(Event::ProgramScene {
            now: t0,
            cam: Some(CamId::Left),
        });
        let base = t0 + Duration::from_secs(5);
        let tick = |sw: &mut Switcher, ms: u64| {
            sw.handle(Event::Tick {
                now: base + Duration::from_millis(ms),
                decision: prefer(CamId::Right),
            })
        };
        assert!(tick(&mut sw, 0).is_none());
        assert!(tick(&mut sw, 799).is_none());
        assert!(tick(&mut sw, 800).is_some());
    }

    #[test]
    fn contrary_evidence_restarts_dwell() {
        let mut r = Rig::new(CamId::Left);
        for interrupt in [
            hold(HoldReason::Ambiguous),
            hold(HoldReason::LowScore),
            hold(HoldReason::LookingDown),
            prefer(CamId::Left),
        ] {
            assert!(r.run(prefer(CamId::Right), 3).is_empty());
            assert!(r.tick(interrupt).is_none());
        }
        // Flicker between right and anything else never switches.
        for _ in 0..40 {
            assert!(r.run(prefer(CamId::Right), 3).is_empty());
            assert!(r.tick(hold(HoldReason::Ambiguous)).is_none());
        }
        assert_eq!(r.sw.current(), Some(CamId::Left));
    }

    #[test]
    fn short_tracking_dropout_keeps_dwell() {
        let mut r = Rig::new(CamId::Left);
        assert!(r.tick(prefer(CamId::Right)).is_none()); // 0 ms: dwell starts
        assert!(r.tick(prefer(CamId::Right)).is_none()); // 250
        assert!(r.tick(hold(HoldReason::TrackingLost)).is_none()); // 500: one dropped frame
        assert!(r.tick(prefer(CamId::Right)).is_none()); // 750
        let cmd = r.tick(prefer(CamId::Right)).expect("1000 ms, 4 votes");
        assert_eq!(cmd.to, CamId::Right);
    }

    #[test]
    fn long_tracking_dropout_resets_dwell() {
        let mut r = Rig::new(CamId::Left);
        assert!(r.run(prefer(CamId::Right), 3).is_empty()); // 0..500
        assert!(r.run(hold(HoldReason::NoFace), 3).is_empty()); // 750..1250: gap >= 500
        // The old dwell is gone: a fresh 800 ms is needed.
        assert!(r.run(prefer(CamId::Right), 4).is_empty());
        assert_eq!(r.run(prefer(CamId::Right), 1).len(), 1);
    }

    #[test]
    fn missing_evidence_alone_never_switches() {
        let mut r = Rig::new(CamId::Left);
        assert!(r.run(prefer(CamId::Right), 3).is_empty()); // 3 votes, 500 ms
        // Dwell time passes during dropouts, but a dropout tick never switches.
        assert!(r.tick(hold(HoldReason::NoFace)).is_none()); // 750
        assert!(r.tick(hold(HoldReason::NoFace)).is_none()); // 1000
        // Next real verdict confirms: switch.
        assert!(r.tick(prefer(CamId::Right)).is_some()); // 1250
    }

    #[test]
    fn strict_mode_resets_on_any_dropout() {
        let t0 = Instant::now();
        let params = SwitchParams {
            max_gap: Duration::ZERO,
            ..Default::default()
        };
        let mut sw = Switcher::new(params, Mode::Auto);
        sw.handle(Event::ProgramScene {
            now: t0,
            cam: Some(CamId::Left),
        });
        let base = t0 + Duration::from_secs(5);
        let seq = [
            prefer(CamId::Right),
            prefer(CamId::Right),
            hold(HoldReason::TrackingLost),
            prefer(CamId::Right),
            prefer(CamId::Right),
            prefer(CamId::Right),
            prefer(CamId::Right),
        ];
        let mut switched = None;
        for (i, d) in seq.into_iter().enumerate() {
            if sw
                .handle(Event::Tick {
                    now: base + TICK * i as u32,
                    decision: d,
                })
                .is_some()
            {
                switched = Some(i);
            }
        }
        // Dwell restarted at index 3: 3,4,5,6 = 750 ms < 800: no switch yet.
        assert_eq!(switched, None);
    }

    #[test]
    fn min_votes_required() {
        let t0 = Instant::now();
        let params = SwitchParams {
            min_votes: 4,
            ..Default::default()
        };
        let mut sw = Switcher::new(params, Mode::Auto);
        sw.handle(Event::ProgramScene {
            now: t0,
            cam: Some(CamId::Left),
        });
        let base = t0 + Duration::from_secs(5);
        let seq = [
            prefer(CamId::Right),           // 0: vote 1
            hold(HoldReason::TrackingLost), // 250
            prefer(CamId::Right),           // 500: vote 2
            hold(HoldReason::TrackingLost), // 750
            prefer(CamId::Right),           // 1000: vote 3, dwell ok, votes not
            prefer(CamId::Right),           // 1250: vote 4 -> switch
        ];
        let mut switched = None;
        for (i, d) in seq.into_iter().enumerate() {
            if switched.is_none()
                && sw
                    .handle(Event::Tick {
                        now: base + TICK * i as u32,
                        decision: d,
                    })
                    .is_some()
            {
                switched = Some(i);
            }
        }
        assert_eq!(switched, Some(5));
    }

    #[test]
    fn min_interval_between_auto_switches() {
        let mut r = Rig::new(CamId::Left);
        let first = r.run(prefer(CamId::Right), 5);
        assert_eq!(first.len(), 1);
        let t_first = first[0].0;
        // Immediately look back: dwell completes at +800 ms, but the interval
        // since the last switch is only ~1050 ms then, so it must wait.
        let back = r.run(prefer(CamId::Left), 12);
        assert_eq!(back.len(), 1);
        let gap = TICK * (back[0].0 - t_first);
        assert!(gap >= Duration::from_millis(1500), "gap {gap:?}");
        assert!(gap < Duration::from_millis(1750), "gap {gap:?}");
    }

    #[test]
    fn holds_never_switch() {
        let mut r = Rig::new(CamId::Right);
        for reason in [
            HoldReason::NoFace,
            HoldReason::LookingDown,
            HoldReason::LowScore,
            HoldReason::Ambiguous,
        ] {
            assert!(r.run(hold(reason), 100).is_empty());
        }
        assert_eq!(r.sw.current(), Some(CamId::Right));
    }

    #[test]
    fn manual_override_wins_and_sticks() {
        let mut r = Rig::new(CamId::Left);
        let now = r.now();
        let cmd =
            r.sw.handle(Event::ManualSelect {
                now,
                cam: CamId::Right,
            })
            .expect("manual switches immediately");
        assert_eq!((cmd.to, cmd.cause), (CamId::Right, SwitchCause::Manual));
        assert_eq!(r.sw.mode(), Mode::Manual);
        // The tracker now strongly prefers Left: ignored in Manual.
        assert!(r.run(prefer(CamId::Left), 40).is_empty());
        assert_eq!(r.sw.current(), Some(CamId::Right));

        // Manual select ignores min interval: two in a row are both honoured.
        let now = r.now();
        assert!(
            r.sw.handle(Event::ManualSelect {
                now,
                cam: CamId::Left
            })
            .is_some()
        );
        assert!(
            r.sw.handle(Event::ManualSelect {
                now,
                cam: CamId::Right
            })
            .is_some()
        );
    }

    #[test]
    fn back_to_auto_needs_fresh_dwell() {
        let mut r = Rig::new(CamId::Left);
        let now = r.now();
        r.sw.handle(Event::ManualSelect {
            now,
            cam: CamId::Right,
        });
        // Tracker prefers Left while in Manual, for a long time.
        assert!(r.run(prefer(CamId::Left), 10).is_empty());
        let now = r.now();
        r.sw.handle(Event::SetMode {
            now,
            mode: Mode::Auto,
        });
        // The time spent preferring Left in Manual does not count.
        let s = r.run(prefer(CamId::Left), 3);
        assert!(s.is_empty(), "{s:?}");
        let s = r.run(prefer(CamId::Left), 3);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].1.to, CamId::Left);
    }

    #[test]
    fn manual_select_respects_following_min_interval() {
        // After a manual switch and returning to Auto, an auto switch still
        // waits for min_interval measured from the manual one.
        let t0 = Instant::now();
        let mut sw = Switcher::new(SwitchParams::default(), Mode::Auto);
        sw.handle(Event::ManualSelect {
            now: t0,
            cam: CamId::Left,
        });
        sw.handle(Event::SetMode {
            now: t0,
            mode: Mode::Auto,
        });
        let mut switched_at = None;
        for i in 0..20u32 {
            let now = t0 + TICK * i;
            if sw
                .handle(Event::Tick {
                    now,
                    decision: prefer(CamId::Right),
                })
                .is_some()
            {
                switched_at = Some(TICK * i);
                break;
            }
        }
        assert_eq!(switched_at, Some(Duration::from_millis(1500)));
    }

    #[test]
    fn external_program_change_is_adopted() {
        let mut r = Rig::new(CamId::Left);
        assert!(r.run(prefer(CamId::Right), 3).is_empty()); // dwell in progress
        let now = r.now();
        r.sw.handle(Event::ProgramScene {
            now,
            cam: Some(CamId::Right),
        });
        assert_eq!(r.sw.current(), Some(CamId::Right));
        assert!(r.sw.pending().is_none());
        // Tracker agrees: nothing to do.
        assert!(r.run(prefer(CamId::Right), 10).is_empty());
    }

    #[test]
    fn switch_failure_restores_previous() {
        let mut r = Rig::new(CamId::Left);
        let s = r.run(prefer(CamId::Right), 5);
        assert_eq!(s.len(), 1);
        let now = r.now();
        r.sw.handle(Event::SwitchFailed {
            now,
            previous: Some(CamId::Left),
        });
        assert_eq!(r.sw.current(), Some(CamId::Left));
        // It will try again after a fresh dwell (and min interval).
        let s = r.run(prefer(CamId::Right), 8);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn unknown_program_scene_then_first_choice() {
        let t0 = Instant::now();
        let mut sw = Switcher::new(SwitchParams::default(), Mode::Auto);
        sw.handle(Event::ProgramScene { now: t0, cam: None });
        let mut out = None;
        for i in 1..=10u32 {
            out = out.or(sw.handle(Event::Tick {
                now: t0 + Duration::from_secs(2) + TICK * i,
                decision: prefer(CamId::Left),
            }));
        }
        assert_eq!(out.map(|c| c.to), Some(CamId::Left));
    }
}

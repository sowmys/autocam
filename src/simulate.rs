//! Synthetic end-to-end simulation of the decision stack.
//!
//! A scripted user moves their head between two monitors, looks down at the
//! keyboard, glances at the gap between monitors and occasionally leaves.
//! Each camera observes the head pose through its own mounting offset, with
//! Gaussian noise, outliers and detection dropouts. The *real* calibration
//! fit, scoring, decision and switching code run on these signals at 4 Hz,
//! and [`EvalTracker`] measures latency and false switches against the script.
//!
//! This measures the decision logic, not the face tracker: the noise model
//! stands in for MediaPipe's jitter. Real-camera numbers come from
//! `autocam run` with ground-truth hotkeys (see docs/ACCEPTANCE.md).

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::diagnostics::{EvalTracker, Summary};
use crate::face::HeadPose;
use crate::scoring::{
    self, CalibrationSamples, CamSignal, DecisionParams, PhaseSamples, fit_calibration,
};
use crate::switching::{Event, Mode, SwitchParams, Switcher};
use crate::{CamId, Cams};

/// SplitMix64: tiny, deterministic, good enough for simulation.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }

    pub fn chance(&mut self, p: f32) -> bool {
        self.unit() < p
    }

    /// Standard normal via Box-Muller.
    pub fn normal(&mut self) -> f32 {
        let u1 = self.unit().max(1e-7);
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gaze {
    /// Facing monitor `i` (the one with camera `i`).
    Monitor(usize),
    Down,
    /// Looking at the gap between monitors `i` and `i + 1`.
    Between(usize),
    Away,
}

impl Gaze {
    fn label(self) -> Option<CamId> {
        match self {
            Gaze::Monitor(i) => Some(CamId(i)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SimParams {
    pub hours: f32,
    pub seed: u64,
    /// Per-sample pose noise, degrees (1 sigma).
    pub noise_deg: f32,
    /// Probability per sample of a gross outlier (15-40 deg).
    pub outlier_p: f32,
    /// Probability per sample that a visible face is not detected.
    pub dropout_p: f32,
    /// Head yaw of each monitor's centre (world frame), degrees, one per
    /// camera, left to right.
    pub monitor_yaw: Vec<f32>,
    /// Extra yaw each camera sees because of its mounting angle.
    pub mount_bias: Vec<f32>,
    pub fps: f32,
}

impl Default for SimParams {
    fn default() -> Self {
        Self::for_cameras(2)
    }
}

impl SimParams {
    /// `n` monitors side by side, 44 degrees apart (two 32" monitors at arm's
    /// length), each camera mounted a few degrees off-axis.
    pub fn for_cameras(n: usize) -> Self {
        let n = n.max(2);
        let span = 44.0 * (n - 1) as f32;
        let monitor_yaw = (0..n).map(|i| span / 2.0 - 44.0 * i as f32).collect();
        let mount_bias = (0..n).map(|i| [4.0, -6.0, 2.0, -3.0][i % 4]).collect();
        Self {
            hours: 2.0,
            seed: 1,
            noise_deg: 3.0,
            outlier_p: 0.02,
            dropout_p: 0.05,
            monitor_yaw,
            mount_bias,
            fps: 4.0,
        }
    }

    pub fn cameras(&self) -> usize {
        self.monitor_yaw.len()
    }
}

struct Head {
    yaw: f32,
    pitch: f32,
}

struct World<'a> {
    p: &'a SimParams,
    rng: Rng,
}

impl World<'_> {
    fn n(&self) -> usize {
        self.p.cameras()
    }

    fn target(&mut self, g: Gaze) -> (f32, f32) {
        let m = &self.p.monitor_yaw;
        let mid = (m[0] + m[m.len() - 1]) / 2.0;
        match g {
            // Users look at different parts of a monitor, not just the centre.
            Gaze::Monitor(i) => (m[i] + self.rng.range(-6.0, 6.0), self.rng.range(-5.0, 4.0)),
            Gaze::Down => (
                mid + self.rng.range(-10.0, 10.0),
                self.rng.range(-40.0, -28.0),
            ),
            Gaze::Between(i) => (
                (m[i] + m[i + 1]) / 2.0 + self.rng.range(-4.0, 4.0),
                self.rng.range(-3.0, 3.0),
            ),
            Gaze::Away => (if self.rng.chance(0.5) { 120.0 } else { -120.0 }, 0.0),
        }
    }

    /// What camera `cam` reports for this head pose (None = no face).
    fn observe(&mut self, cam: CamId, h: &Head) -> Option<HeadPose> {
        let rel_yaw = h.yaw - self.p.monitor_yaw[cam.0] + self.p.mount_bias[cam.0];
        if rel_yaw.abs() > 60.0 || self.rng.chance(self.p.dropout_p) {
            return None;
        }
        let mut yaw = rel_yaw + self.p.noise_deg * self.rng.normal();
        let mut pitch = h.pitch + self.p.noise_deg * self.rng.normal();
        if self.rng.chance(self.p.outlier_p) {
            let s = if self.rng.chance(0.5) { 1.0 } else { -1.0 };
            yaw += s * self.rng.range(15.0, 40.0);
            pitch += self.rng.range(-10.0, 10.0);
        }
        Some(HeadPose {
            yaw,
            pitch,
            roll: 0.0,
        })
    }

    fn pick(&mut self, n: usize) -> usize {
        ((self.rng.unit() * n as f32) as usize).min(n - 1)
    }

    fn next_gaze(&mut self, g: Gaze) -> (Gaze, f32) {
        let n = self.n();
        let r = self.rng.unit();
        let next = match g {
            Gaze::Monitor(i) => {
                if r < 0.55 {
                    // Another monitor, uniformly.
                    let j = self.pick(n - 1);
                    Gaze::Monitor(if j >= i { j + 1 } else { j })
                } else if r < 0.80 {
                    Gaze::Down
                } else if r < 0.97 {
                    // A gap next to the current monitor.
                    let gaps: Vec<usize> = [i.checked_sub(1), (i + 1 < n).then_some(i)]
                        .into_iter()
                        .flatten()
                        .collect();
                    Gaze::Between(gaps[self.pick(gaps.len())])
                } else {
                    Gaze::Away
                }
            }
            _ => Gaze::Monitor(self.pick(n)),
        };
        let dur = match next {
            Gaze::Monitor(_) => self.rng.range(3.0, 30.0),
            Gaze::Down => self.rng.range(0.8, 8.0),
            Gaze::Between(_) => self.rng.range(0.3, 1.2),
            Gaze::Away => self.rng.range(2.0, 12.0),
        };
        (next, dur)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SimResult {
    pub params: SimParams,
    pub simulated_hours: f32,
    pub label_changes: u64,
    pub auto_switches: u64,
    pub correct_switches: u64,
    pub false_switches: u64,
    pub false_switches_per_hour: f64,
    pub missed_switches: u64,
    /// Label change (start of head turn) -> switch.
    pub latency_ms: Option<Summary>,
    pub calibration_warnings: Vec<String>,
}

pub fn simulate(p: &SimParams, decision: DecisionParams, switch: SwitchParams) -> SimResult {
    let n = p.cameras();
    let mut w = World {
        p,
        rng: Rng::new(p.seed),
    };
    let dt = Duration::from_secs_f32(1.0 / p.fps);

    // 1. Calibrate with the same noisy observation model, 3 s per phase.
    let mut phase = |yaw: f32, pitch: f32| {
        let h = Head { yaw, pitch };
        let mut s = PhaseSamples::new(n);
        for _ in 0..(3.0 * p.fps) as usize {
            s.frames += 1;
            for c in 0..n {
                if let Some(o) = w.observe(CamId(c), &h) {
                    s.poses.get_mut(CamId(c)).push(o);
                }
            }
        }
        s
    };
    let mid = (p.monitor_yaw[0] + p.monitor_yaw[n - 1]) / 2.0;
    let samples = CalibrationSamples {
        phases: p.monitor_yaw.iter().map(|&y| phase(y, 0.0)).collect(),
        look_down: Some(phase(mid, -34.0)),
    };
    let (cal, report) = fit_calibration(&samples, decision.margin, 0)
        .expect("synthetic calibration should succeed");

    // 2. Run the scripted session.
    let t0 = Instant::now();
    let mut now = t0;
    let end = t0 + Duration::from_secs_f32(p.hours * 3600.0);
    let mut sw = Switcher::new(switch, Mode::Auto);
    sw.handle(Event::ProgramScene {
        now,
        cam: Some(CamId(0)),
    });
    let mut eval = EvalTracker::new(Duration::from_secs(5));
    let mut gaze = Gaze::Monitor(0);
    eval.on_label(now, gaze.label(), sw.current());
    let mut seg_end = now + Duration::from_secs(10);
    let (ty, tp) = w.target(gaze);
    let mut head = Head { yaw: ty, pitch: tp };
    let mut from = (head.yaw, head.pitch);
    let mut to = from;
    let mut move_start = now;
    // Turning further takes longer: ~0.35 s per 44 deg.
    let mut move_time = 0.35f32;
    let mut auto = 0u64;

    while now < end {
        now += dt;
        if now >= seg_end {
            let (g, dur) = w.next_gaze(gaze);
            gaze = g;
            seg_end = now + Duration::from_secs_f32(dur);
            from = (head.yaw, head.pitch);
            to = w.target(g);
            move_time = (0.35 * (to.0 - from.0).abs() / 44.0).clamp(0.2, 0.8);
            move_start = now;
            eval.on_label(now, gaze.label(), sw.current());
        }
        let a = (now.saturating_duration_since(move_start).as_secs_f32() / move_time).min(1.0);
        head.yaw = from.0 + (to.0 - from.0) * a;
        head.pitch = from.1 + (to.1 - from.1) * a;
        // Small natural sway while holding a pose.
        head.yaw += 0.8 * w.rng.normal();
        head.pitch += 0.8 * w.rng.normal();

        let signals = Cams(
            (0..n)
                .map(|c| match w.observe(CamId(c), &head) {
                    Some(pose) => {
                        CamSignal::Face(scoring::score_pose(&cal.camera(CamId(c)), &pose))
                    }
                    None => CamSignal::NoFace,
                })
                .collect(),
        );
        let d = scoring::decide(&signals, decision);
        if let Some(cmd) = sw.handle(Event::Tick { now, decision: d }) {
            auto += 1;
            eval.on_switch(now, cmd.to, cmd.cause);
        }
        eval.on_tick(now);
    }

    let s = &eval.stats;
    SimResult {
        params: p.clone(),
        simulated_hours: p.hours,
        label_changes: s.label_changes,
        auto_switches: auto,
        correct_switches: s.correct_switches,
        false_switches: s.false_switches,
        false_switches_per_hour: s.false_switches as f64 / f64::from(p.hours),
        missed_switches: s.missed_switches,
        latency_ms: s.latency_ms.summary(),
        calibration_warnings: report.warnings,
    }
}

pub fn print_results(results: &[SimResult]) {
    println!(
        "\n{:>4} {:>6} {:>8} {:>8} {:>6} | {:>8} {:>8} {:>8} {:>9} {:>7} | {:>8} {:>8} {:>8}",
        "cams",
        "noise",
        "outlier",
        "dropout",
        "hours",
        "labels",
        "switches",
        "correct",
        "false/h",
        "missed",
        "lat p50",
        "lat p95",
        "lat max"
    );
    for r in results {
        let (p50, p95, max) = r
            .latency_ms
            .map_or((f32::NAN, f32::NAN, f32::NAN), |l| (l.p50, l.p95, l.max));
        println!(
            "{:>4} {:>5.1}° {:>7.1}% {:>7.1}% {:>6.1} | {:>8} {:>8} {:>8} {:>9.2} {:>7} | {:>6.0}ms {:>6.0}ms {:>6.0}ms",
            r.params.cameras(),
            r.params.noise_deg,
            100.0 * r.params.outlier_p,
            100.0 * r.params.dropout_p,
            r.simulated_hours,
            r.label_changes,
            r.auto_switches,
            r.correct_switches,
            r.false_switches_per_hour,
            r.missed_switches,
            p50,
            p95,
            max
        );
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> (DecisionParams, SwitchParams) {
        (
            DecisionParams {
                min_score: 0.45,
                margin: 0.25,
            },
            SwitchParams::default(),
        )
    }

    #[test]
    fn rng_is_deterministic_and_roughly_normal() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        assert_eq!(a.next_u64(), b.next_u64());
        let n = 20_000;
        let xs: Vec<f32> = (0..n).map(|_| a.normal()).collect();
        let mean = xs.iter().sum::<f32>() / n as f32;
        let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n as f32;
        assert!(mean.abs() < 0.05, "{mean}");
        assert!((var - 1.0).abs() < 0.08, "{var}");
    }

    #[test]
    fn simulated_session_meets_targets() {
        let (d, s) = defaults();
        let r = simulate(
            &SimParams {
                hours: 0.5,
                ..Default::default()
            },
            d,
            s,
        );
        assert!(r.correct_switches > 20, "{r:?}");
        assert!(r.false_switches_per_hour < 2.0, "{r:?}");
        let lat = r.latency_ms.expect("some switches");
        // Dwell 800 ms + head turn + tick quantization.
        assert!(lat.p50 >= 800.0 && lat.p50 < 1600.0, "{lat:?}");
    }

    #[test]
    fn three_camera_session_meets_targets() {
        let (d, s) = defaults();
        let p = SimParams {
            hours: 0.5,
            ..SimParams::for_cameras(3)
        };
        assert_eq!(p.monitor_yaw, vec![44.0, 0.0, -44.0]);
        let r = simulate(&p, d, s);
        assert!(r.correct_switches > 20, "{r:?}");
        assert!(r.false_switches_per_hour < 2.0, "{r:?}");
        assert!(r.calibration_warnings.is_empty(), "{r:?}");
        let lat = r.latency_ms.expect("some switches");
        assert!(lat.p50 >= 800.0 && lat.p50 < 1800.0, "{lat:?}");
    }

    #[test]
    fn noisy_disconnected_like_signals_do_not_flap() {
        // Heavy dropouts (camera flaking) and outliers: should mostly hold.
        let (d, s) = defaults();
        let r = simulate(
            &SimParams {
                hours: 0.5,
                dropout_p: 0.4,
                outlier_p: 0.1,
                noise_deg: 5.0,
                seed: 3,
                ..Default::default()
            },
            d,
            s,
        );
        assert!(r.false_switches_per_hour < 6.0, "{r:?}");
    }
}

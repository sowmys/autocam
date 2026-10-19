//! Calibration, per-camera frontality scores, and the per-tick decision.
//!
//! Each camera is calibrated independently. When the user looks at a
//! camera's monitor, that camera sees some head pose (the *target*). It is
//! rarely (0, 0): cameras sit on top of / beside the monitor and point at the
//! user from an angle. Scores are a Gaussian of the angular distance from the
//! target, with a width derived from how far apart "looking at my monitor" and
//! "looking at the nearest other monitor" are *for that camera*. That
//! normalizes all cameras onto a comparable 0-1 scale regardless of mounting
//! angle, for any number of cameras.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::capture::SignalFault;
use crate::face::{FaceObservation, HeadPose};
use crate::{CamId, Cams};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PoseTarget {
    pub yaw: f32,
    pub pitch: f32,
}

/// One camera's mapping from observed head pose to "user faces my monitor".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraCalibration {
    /// Pose this camera observes when the user looks at its monitor.
    pub target: PoseTarget,
    /// Pose this camera observes when the user looks at the nearest other
    /// monitor, if it could still see the face.
    pub away: Option<PoseTarget>,
    /// Gaussian widths, degrees.
    pub sigma_yaw: f32,
    pub sigma_pitch: f32,
    /// Pitch relative to `target.pitch` below which the user is looking down
    /// (keyboard, notes, phone). Negative degrees.
    pub down_pitch: f32,
}

impl Default for CameraCalibration {
    /// Uncalibrated: camera on its monitor, pointing at the user.
    fn default() -> Self {
        Self {
            target: PoseTarget {
                yaw: 0.0,
                pitch: 0.0,
            },
            away: None,
            sigma_yaw: 12.0,
            sigma_pitch: 15.0,
            down_pitch: -20.0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    /// One entry per configured camera, in camera order.
    pub cameras: Vec<CameraCalibration>,
    /// Unix seconds.
    pub created_unix: u64,
}

impl Calibration {
    /// The camera's calibration, or the uncalibrated default.
    pub fn camera(&self, cam: CamId) -> CameraCalibration {
        self.cameras.get(cam.0).cloned().unwrap_or_default()
    }

    /// True if it was made for `n` cameras.
    pub fn fits(&self, n: usize) -> bool {
        self.cameras.len() == n
    }
}

/// A camera's score for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CamScore {
    /// 0..1, 1 = exactly the calibrated "looking at my monitor" pose.
    pub score: f32,
    pub d_yaw: f32,
    pub d_pitch: f32,
    pub looking_down: bool,
}

pub fn score(cal: &CameraCalibration, obs: &FaceObservation) -> CamScore {
    score_pose(cal, &obs.pose)
}

pub fn score_pose(cal: &CameraCalibration, pose: &HeadPose) -> CamScore {
    let d_yaw = pose.yaw - cal.target.yaw;
    let d_pitch = pose.pitch - cal.target.pitch;
    let zy = d_yaw / cal.sigma_yaw.max(1.0);
    let zp = d_pitch / cal.sigma_pitch.max(1.0);
    CamScore {
        score: (-0.5 * (zy * zy + zp * zp)).exp(),
        d_yaw,
        d_pitch,
        looking_down: d_pitch < cal.down_pitch,
    }
}

/// What one camera contributed this tick.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CamSignal {
    Face(CamScore),
    /// Frame was fine, but no face was found in it.
    NoFace,
    /// Frame unusable (capture failed, black, frozen).
    Fault {
        fault: SignalFault,
    },
}

impl CamSignal {
    pub fn score(&self) -> Option<f32> {
        match self {
            CamSignal::Face(s) => Some(s.score),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    /// No camera found a face.
    NoFace,
    /// Some cameras lost the face and the rest are not conclusive: missing
    /// evidence, not contrary evidence.
    TrackingLost,
    /// The user is looking down (keyboard, notes).
    LookingDown,
    /// All cameras see the face and none is frontal enough.
    LowScore,
    /// The best two cameras score similarly.
    Ambiguous,
    /// Mouse detector: cursor is on a display that belongs to no camera.
    UnmappedMonitor,
    /// Mouse detector: cursor position unavailable (lock screen, UAC prompt).
    CursorUnknown,
    /// Mouse detector: the selected camera has a capture fault.
    TargetUnavailable,
}

impl HoldReason {
    /// Neutral holds come from missing data (tracking dropouts, faults).
    /// They never cause a switch, but a short gap does not throw away a dwell
    /// that is otherwise uncontradicted. All other holds are contrary
    /// evidence and restart the dwell.
    pub fn is_neutral(self) -> bool {
        matches!(
            self,
            HoldReason::NoFace
                | HoldReason::TrackingLost
                | HoldReason::CursorUnknown
                | HoldReason::TargetUnavailable
        )
    }
}

/// The per-tick verdict that feeds the switching state machine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    Prefer { cam: CamId, score: f32, margin: f32 },
    Hold { reason: HoldReason },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecisionParams {
    pub min_score: f32,
    pub margin: f32,
}

/// Prefers the most frontal camera if it is frontal enough and beats every
/// other camera (the runner-up) by the margin. Cameras without a face count
/// as score 0 and can never be preferred.
pub fn decide(signals: &Cams<CamSignal>, p: DecisionParams) -> Decision {
    let faces: Vec<(CamId, CamScore)> = signals
        .iter()
        .filter_map(|(c, s)| match s {
            CamSignal::Face(s) => Some((c, *s)),
            _ => None,
        })
        .collect();
    if faces.is_empty() {
        return Decision::Hold {
            reason: HoldReason::NoFace,
        };
    }
    if faces.iter().any(|(_, s)| s.looking_down) {
        return Decision::Hold {
            reason: HoldReason::LookingDown,
        };
    }
    let (best, best_score) = faces
        .iter()
        .max_by(|a, b| a.1.score.total_cmp(&b.1.score))
        .map(|(c, s)| (*c, s.score))
        .expect("faces is non-empty");
    let runner_up = faces
        .iter()
        .filter(|(c, _)| *c != best)
        .map(|(_, s)| s.score)
        .fold(0.0f32, f32::max);
    if best_score < p.min_score {
        let reason = if faces.len() < signals.len() {
            HoldReason::TrackingLost
        } else {
            HoldReason::LowScore
        };
        return Decision::Hold { reason };
    }
    let margin = best_score - runner_up;
    if margin < p.margin {
        return Decision::Hold {
            reason: HoldReason::Ambiguous,
        };
    }
    Decision::Prefer {
        cam: best,
        score: best_score,
        margin,
    }
}

// ---------------------------------------------------------------------------
// Calibration fitting
// ---------------------------------------------------------------------------

/// Poses each camera observed during one calibration phase, plus how many
/// frames were sampled in that phase (for detection-rate checks).
#[derive(Debug, Clone, Default)]
pub struct PhaseSamples {
    pub poses: Cams<Vec<HeadPose>>,
    pub frames: usize,
}

impl PhaseSamples {
    pub fn new(cameras: usize) -> Self {
        Self {
            poses: Cams::filled(cameras, Vec::new()),
            frames: 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CalibrationSamples {
    /// Phase `i`: the user faces the centre of camera `i`'s monitor.
    pub phases: Vec<PhaseSamples>,
    /// Optional: the user looks down at the keyboard.
    pub look_down: Option<PhaseSamples>,
}

#[derive(Debug, Error, PartialEq)]
pub enum CalibrationError {
    #[error(
        "{cam} saw your face in only {seen} of {frames} frames while you looked at its monitor; check lighting and that the camera points at you"
    )]
    FaceNotSeen {
        cam: CamId,
        seen: usize,
        frames: usize,
    },
    #[error(
        "{cam}: looking at its monitor and at {other}'s gives nearly the same head pose ({separation:.1} deg apart); the monitors may be too close together, or you moved only your eyes. Turn your head towards each monitor."
    )]
    NoSeparation {
        cam: CamId,
        other: CamId,
        separation: f32,
    },
    #[error("calibration needs one phase per camera ({cameras} cameras, {phases} phases)")]
    PhaseCount { cameras: usize, phases: usize },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalibrationReport {
    /// Per camera: angular distance between "own" and the nearest "away"
    /// pose (deg); `None` if the camera lost the face whenever the user
    /// looked at another monitor.
    pub separation: Vec<Option<f32>>,
    /// Per camera: noise (robust std-dev while holding still), deg.
    pub noise: Vec<f32>,
    /// `predicted[phase][cam]`: score at the median pose of each phase.
    pub predicted: Vec<Vec<f32>>,
    pub warnings: Vec<String>,
}

const MIN_DETECTION_RATE: f32 = 0.4;
const MIN_AWAY_RATE: f32 = 0.3;
const MIN_SEPARATION_DEG: f32 = 6.0;
const WEAK_SEPARATION_DEG: f32 = 12.0;

fn median(v: &mut [f32]) -> f32 {
    v.sort_by(f32::total_cmp);
    let n = v.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

fn median_pose(poses: &[HeadPose]) -> PoseTarget {
    let mut y: Vec<f32> = poses.iter().map(|p| p.yaw).collect();
    let mut p: Vec<f32> = poses.iter().map(|p| p.pitch).collect();
    PoseTarget {
        yaw: median(&mut y),
        pitch: median(&mut p),
    }
}

/// Robust standard deviation (1.4826 * MAD).
fn robust_std(values: impl Iterator<Item = f32>) -> f32 {
    let mut v: Vec<f32> = values.collect();
    let m = median(&mut v);
    let mut dev: Vec<f32> = v.iter().map(|x| (x - m).abs()).collect();
    1.4826 * median(&mut dev)
}

fn rate(seen: usize, frames: usize) -> f32 {
    if frames == 0 {
        0.0
    } else {
        seen as f32 / frames as f32
    }
}

fn seen_enough(poses: &[HeadPose], frames: usize, min_rate: f32) -> bool {
    rate(poses.len(), frames) >= min_rate && poses.len() >= 3
}

pub fn fit_calibration(
    s: &CalibrationSamples,
    margin: f32,
    now_unix: u64,
) -> Result<(Calibration, CalibrationReport), CalibrationError> {
    let n = s.phases.len();
    if n < 2 || s.phases.iter().any(|p| p.poses.len() != n) {
        return Err(CalibrationError::PhaseCount {
            cameras: s.phases.first().map_or(0, |p| p.poses.len()),
            phases: n,
        });
    }
    let mut cams = Vec::with_capacity(n);
    let mut separation = vec![None; n];
    let mut noise = vec![0.0; n];
    let mut warnings = Vec::new();

    for i in 0..n {
        let cam = CamId(i);
        let own = &s.phases[i];
        let own_poses = own.poses.get(cam);
        if !seen_enough(own_poses, own.frames, MIN_DETECTION_RATE) {
            return Err(CalibrationError::FaceNotSeen {
                cam,
                seen: own_poses.len(),
                frames: own.frames,
            });
        }
        let target = median_pose(own_poses);
        let nz = robust_std(own_poses.iter().map(|p| p.yaw))
            .max(robust_std(own_poses.iter().map(|p| p.pitch)));
        noise[i] = nz;

        // The nearest other monitor (as this camera sees it) sets the width:
        // it is the one hardest to tell apart.
        let nearest = s
            .phases
            .iter()
            .enumerate()
            .filter(|(j, p)| *j != i && seen_enough(p.poses.get(cam), p.frames, MIN_AWAY_RATE))
            .map(|(j, p)| {
                let a = median_pose(p.poses.get(cam));
                (j, a, (a.yaw - target.yaw).hypot(a.pitch - target.pitch))
            })
            .min_by(|a, b| a.2.total_cmp(&b.2));

        let sigma_yaw = match nearest {
            Some((j, _, sep)) => {
                separation[i] = Some(sep);
                if sep < MIN_SEPARATION_DEG {
                    return Err(CalibrationError::NoSeparation {
                        cam,
                        other: CamId(j),
                        separation: sep,
                    });
                }
                if sep < WEAK_SEPARATION_DEG {
                    warnings.push(format!(
                        "{cam}: only {sep:.1} deg between its monitor and {}'s; expect slower or less reliable switching",
                        CamId(j)
                    ));
                }
                // At the "away" pose the score is exp(-0.5 * 3^2) ~= 0.01;
                // half-way it is ~0.32.
                (sep / 3.0).clamp(4.0, 20.0)
            }
            // The face disappears whenever looking away: plenty of separation.
            None => 10.0,
        };
        // Never narrower than the measurement noise allows.
        let sigma_yaw = sigma_yaw.max(2.5 * nz).min(25.0);
        if nz > 6.0 {
            warnings.push(format!(
                "{cam}: head pose is noisy ({nz:.1} deg); improve lighting or move closer"
            ));
        }

        let mut down_pitch = CameraCalibration::default().down_pitch;
        if let Some(down) = &s.look_down {
            let dp = down.poses.get(cam);
            if seen_enough(dp, down.frames, MIN_AWAY_RATE) {
                let rel = median_pose(dp).pitch - target.pitch;
                if rel < -8.0 {
                    // Trigger a bit before reaching the full keyboard pose.
                    down_pitch = (0.6 * rel).clamp(-30.0, -6.0);
                } else {
                    warnings.push(format!(
                        "{cam}: looking down changed pitch by only {rel:.1} deg; keeping default threshold"
                    ));
                }
            }
        }

        cams.push(CameraCalibration {
            target,
            away: nearest.map(|(_, a, _)| a),
            sigma_yaw,
            sigma_pitch: (sigma_yaw * 1.25).clamp(6.0, 25.0),
            down_pitch,
        });
    }

    let cal = Calibration {
        cameras: cams,
        created_unix: now_unix,
    };

    // Predict scores at each phase's median pose. A camera that lost the
    // face in that phase scores 0.
    let predicted: Vec<Vec<f32>> = s
        .phases
        .iter()
        .map(|phase| {
            (0..n)
                .map(|c| {
                    let poses = phase.poses.get(CamId(c));
                    if poses.len() < 3 {
                        return 0.0;
                    }
                    let t = median_pose(poses);
                    let pose = HeadPose {
                        yaw: t.yaw,
                        pitch: t.pitch,
                        roll: 0.0,
                    };
                    score_pose(&cal.camera(CamId(c)), &pose).score
                })
                .collect()
        })
        .collect();
    for (i, row) in predicted.iter().enumerate() {
        let runner_up = row
            .iter()
            .enumerate()
            .filter(|(c, _)| *c != i)
            .map(|(_, v)| *v)
            .fold(0.0f32, f32::max);
        if row[i] - runner_up < margin {
            warnings.push(format!(
                "facing {}'s monitor: predicted margin {:.2} is below the configured {margin:.2}",
                CamId(i),
                row[i] - runner_up
            ));
        }
    }

    Ok((
        cal,
        CalibrationReport {
            separation,
            noise,
            predicted,
            warnings,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::face::PoseMethod;

    const L: CamId = CamId(0);
    const R: CamId = CamId(1);

    fn pose(yaw: f32, pitch: f32) -> HeadPose {
        HeadPose {
            yaw,
            pitch,
            roll: 0.0,
        }
    }

    fn obs(yaw: f32, pitch: f32) -> FaceObservation {
        FaceObservation {
            pose: pose(yaw, pitch),
            eye_span: 0.1,
            method: PoseMethod::TransformMatrix,
        }
    }

    const P: DecisionParams = DecisionParams {
        min_score: 0.45,
        margin: 0.25,
    };

    fn face(cal: &CameraCalibration, yaw: f32, pitch: f32) -> CamSignal {
        CamSignal::Face(score(cal, &obs(yaw, pitch)))
    }

    fn two(a: CamSignal, b: CamSignal) -> Cams<CamSignal> {
        Cams(vec![a, b])
    }

    /// Deterministic jitter so tests are reproducible without a RNG crate.
    fn jitter(i: usize, amp: f32) -> f32 {
        let x = ((i as f32 * 12.9898).sin() * 43_758.547).fract();
        (x - 0.5) * 2.0 * amp
    }

    /// One phase; `per_cam[c]` are camera c's observed (yaw, pitch) samples.
    fn phase(per_cam: &[&[(f32, f32)]], frames: usize) -> PhaseSamples {
        PhaseSamples {
            poses: Cams(
                per_cam
                    .iter()
                    .map(|v| v.iter().map(|&(y, p)| pose(y, p)).collect())
                    .collect(),
            ),
            frames,
        }
    }

    fn noisy(n: usize, yaw: f32, pitch: f32, amp: f32) -> Vec<(f32, f32)> {
        (0..n)
            .map(|i| (yaw + jitter(i, amp), pitch + jitter(i + 1000, amp)))
            .collect()
    }

    #[test]
    fn score_peaks_at_target_and_falls_off() {
        let cal = CameraCalibration::default();
        assert!((score(&cal, &obs(0.0, 0.0)).score - 1.0).abs() < 1e-6);
        let one_sigma = score(&cal, &obs(cal.sigma_yaw, 0.0)).score;
        assert!((one_sigma - (-0.5f32).exp()).abs() < 1e-4);
        assert!(score(&cal, &obs(40.0, 0.0)).score < 0.01);
        assert!(score(&cal, &obs(0.0, -25.0)).looking_down);
    }

    #[test]
    fn decide_prefers_clear_winner() {
        let cal = CameraCalibration::default();
        let s = two(face(&cal, 2.0, 0.0), face(&cal, 35.0, 0.0));
        match decide(&s, P) {
            Decision::Prefer { cam, .. } => assert_eq!(cam, L),
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn decide_holds_when_ambiguous() {
        // Looking half-way between the two monitors: both score similarly.
        let cal = CameraCalibration::default();
        let s = two(face(&cal, 10.0, 0.0), face(&cal, -11.0, 0.0));
        assert_eq!(
            decide(&s, P),
            Decision::Hold {
                reason: HoldReason::Ambiguous
            }
        );
    }

    #[test]
    fn decide_holds_when_both_low() {
        let cal = CameraCalibration::default();
        let s = two(face(&cal, 30.0, 0.0), face(&cal, 40.0, 0.0));
        assert_eq!(
            decide(&s, P),
            Decision::Hold {
                reason: HoldReason::LowScore
            }
        );
    }

    #[test]
    fn decide_holds_when_looking_down_even_if_one_is_frontal() {
        let cal = CameraCalibration::default();
        let s = two(face(&cal, 0.0, -2.0), face(&cal, 30.0, -30.0));
        assert_eq!(
            decide(&s, P),
            Decision::Hold {
                reason: HoldReason::LookingDown
            }
        );
    }

    #[test]
    fn decide_no_face_and_faults() {
        let f = CamSignal::Fault {
            fault: SignalFault::CaptureFailed,
        };
        for s in [
            two(CamSignal::NoFace, CamSignal::NoFace),
            two(f, CamSignal::NoFace),
            two(f, f),
        ] {
            assert_eq!(
                decide(&s, P),
                Decision::Hold {
                    reason: HoldReason::NoFace
                }
            );
        }
    }

    #[test]
    fn decide_one_camera_disconnected_other_frontal() {
        // Right camera unplugged; user clearly faces left camera: prefer left.
        let cal = CameraCalibration::default();
        let s = two(
            face(&cal, 1.0, 0.0),
            CamSignal::Fault {
                fault: SignalFault::Black,
            },
        );
        assert!(matches!(decide(&s, P), Decision::Prefer { cam: L, .. }));
        // ... but a disconnected camera can never be preferred, and a
        // non-frontal view from the remaining camera is missing evidence.
        let s = two(
            face(&cal, 35.0, 0.0),
            CamSignal::Fault {
                fault: SignalFault::Stale,
            },
        );
        assert_eq!(
            decide(&s, P),
            Decision::Hold {
                reason: HoldReason::TrackingLost
            }
        );
    }

    #[test]
    fn decide_three_cameras_uses_runner_up() {
        let cal = CameraCalibration::default();
        // Middle camera frontal, neighbours turned away: prefer middle.
        let s = Cams(vec![
            face(&cal, 30.0, 0.0),
            face(&cal, 1.0, 0.0),
            face(&cal, -30.0, 0.0),
        ]);
        assert!(matches!(
            decide(&s, P),
            Decision::Prefer { cam: CamId(1), .. }
        ));
        // Between cameras 1 and 2: ambiguous against the runner-up even
        // though camera 0 is far behind.
        let s = Cams(vec![
            face(&cal, 40.0, 0.0),
            face(&cal, 9.0, 0.0),
            face(&cal, -10.0, 0.0),
        ]);
        assert_eq!(
            decide(&s, P),
            Decision::Hold {
                reason: HoldReason::Ambiguous
            }
        );
        // One of three without a face, others not frontal: missing evidence.
        let s = Cams(vec![
            face(&cal, 30.0, 0.0),
            CamSignal::NoFace,
            face(&cal, -30.0, 0.0),
        ]);
        assert_eq!(
            decide(&s, P),
            Decision::Hold {
                reason: HoldReason::TrackingLost
            }
        );
    }

    /// Typical angled mounting: the left camera sits left of the user and
    /// sees +25 deg yaw when the user faces the left monitor, the right one
    /// sees -20 deg. Neither target is (0,0).
    fn angled_samples() -> CalibrationSamples {
        CalibrationSamples {
            phases: vec![
                phase(
                    &[
                        &noisy(12, 3.0, -4.0, 1.5),  // left cam: nearly frontal
                        &noisy(12, 48.0, -2.0, 2.0), // right cam: strongly turned
                    ],
                    12,
                ),
                phase(
                    &[
                        &noisy(12, -42.0, -5.0, 2.0), // left cam: strongly turned
                        &noisy(12, 6.0, -1.0, 1.5),   // right cam: nearly frontal
                    ],
                    12,
                ),
            ],
            look_down: Some(phase(
                &[&noisy(12, 0.0, -32.0, 2.0), &noisy(12, 5.0, -28.0, 2.0)],
                12,
            )),
        }
    }

    #[test]
    fn calibration_maps_angled_cameras() {
        let (cal, report) = fit_calibration(&angled_samples(), 0.25, 1).unwrap();
        let (l, r) = (cal.camera(L), cal.camera(R));
        assert!((l.target.yaw - 3.0).abs() < 1.5, "{cal:?}");
        assert!((r.target.yaw - 6.0).abs() < 1.5, "{cal:?}");
        assert!(l.away.is_some() && r.away.is_some());
        // ~45 deg separation -> sigma ~15.
        assert!((10.0..=20.0).contains(&l.sigma_yaw), "{cal:?}");
        // Looking down was ~-28 relative: threshold partway there.
        assert!((-20.0..=-12.0).contains(&l.down_pitch), "{cal:?}");
        assert!(report.predicted[0][0] > 0.9 && report.predicted[0][1] < 0.05);
        assert!(report.predicted[1][1] > 0.9 && report.predicted[1][0] < 0.05);
        assert!(report.warnings.is_empty(), "{report:?}");
    }

    #[test]
    fn calibrated_scores_drive_correct_decisions() {
        let (cal, _) = fit_calibration(&angled_samples(), 0.25, 1).unwrap();
        let sig = |ly: f32, lp: f32, ry: f32, rp: f32| {
            two(
                CamSignal::Face(score(&cal.camera(L), &obs(ly, lp))),
                CamSignal::Face(score(&cal.camera(R), &obs(ry, rp))),
            )
        };
        assert!(matches!(
            decide(&sig(4.0, -3.0, 46.0, -2.0), P),
            Decision::Prefer { cam: L, .. }
        ));
        assert!(matches!(
            decide(&sig(-40.0, -5.0, 7.0, -1.0), P),
            Decision::Prefer { cam: R, .. }
        ));
        // Half-way between: hold.
        assert!(matches!(
            decide(&sig(-20.0, -4.0, 26.0, -2.0), P),
            Decision::Hold { .. }
        ));
        // Keyboard: hold.
        assert_eq!(
            decide(&sig(0.0, -30.0, 5.0, -27.0), P),
            Decision::Hold {
                reason: HoldReason::LookingDown
            }
        );
    }

    #[test]
    fn calibration_three_cameras_uses_nearest_monitor() {
        // Monitors at head yaw +35 (left), 0 (centre), -35 (right); each
        // camera sees yaw = head - monitor. The centre camera's nearest other
        // monitor is 35 deg away; the left camera's nearest is the centre.
        let mons = [35.0f32, 0.0, -35.0];
        let phases = mons
            .iter()
            .map(|&head| {
                let per: Vec<Vec<(f32, f32)>> = mons
                    .iter()
                    .map(|&m| noisy(12, head - m, 0.0, 1.0))
                    .collect();
                let refs: Vec<&[(f32, f32)]> = per.iter().map(Vec::as_slice).collect();
                phase(&refs, 12)
            })
            .collect();
        let s = CalibrationSamples {
            phases,
            look_down: None,
        };
        let (cal, report) = fit_calibration(&s, 0.25, 1).unwrap();
        assert_eq!(cal.cameras.len(), 3);
        for (i, sep) in report.separation.iter().enumerate() {
            let sep = sep.expect("face visible");
            assert!((sep - 35.0).abs() < 3.0, "camera {i}: {sep}");
        }
        // Each phase is won by its own camera, clearly.
        for (i, row) in report.predicted.iter().enumerate() {
            let best = (0..3).max_by(|a, b| row[*a].total_cmp(&row[*b])).unwrap();
            assert_eq!(best, i, "{row:?}");
        }
        assert!(report.warnings.is_empty(), "{report:?}");
    }

    #[test]
    fn calibration_handles_face_lost_when_looking_away() {
        let mut s = angled_samples();
        s.phases[0].poses.get_mut(R).clear(); // right cam loses face when looking left
        let (cal, report) = fit_calibration(&s, 0.25, 1).unwrap();
        assert!(cal.camera(R).away.is_none());
        assert_eq!(cal.camera(R).sigma_yaw, 10.0);
        assert_eq!(report.separation[1], None);
    }

    #[test]
    fn calibration_rejects_missing_face() {
        let mut s = angled_samples();
        s.phases[0].poses.get_mut(L).truncate(2);
        assert!(matches!(
            fit_calibration(&s, 0.25, 1),
            Err(CalibrationError::FaceNotSeen { cam: L, .. })
        ));
    }

    #[test]
    fn calibration_rejects_no_separation() {
        // Eyes moved, head did not: both phases give the same pose.
        let same = noisy(12, 0.0, 0.0, 1.0);
        let s = CalibrationSamples {
            phases: vec![phase(&[&same, &same], 12), phase(&[&same, &same], 12)],
            look_down: None,
        };
        assert!(matches!(
            fit_calibration(&s, 0.25, 1),
            Err(CalibrationError::NoSeparation { .. })
        ));
    }

    #[test]
    fn calibration_rejects_wrong_phase_count() {
        let mut s = angled_samples();
        s.phases.truncate(1);
        assert!(matches!(
            fit_calibration(&s, 0.25, 1),
            Err(CalibrationError::PhaseCount { .. })
        ));
    }

    #[test]
    fn calibration_warns_on_weak_separation() {
        let s = CalibrationSamples {
            phases: vec![
                phase(&[&noisy(12, 0.0, 0.0, 0.5), &noisy(12, 9.0, 0.0, 0.5)], 12),
                phase(&[&noisy(12, -9.0, 0.0, 0.5), &noisy(12, 0.0, 0.0, 0.5)], 12),
            ],
            look_down: None,
        };
        let (_, report) = fit_calibration(&s, 0.25, 1).unwrap();
        assert!(!report.warnings.is_empty());
    }

    #[test]
    fn calibration_roundtrips_through_toml() {
        let (cal, _) = fit_calibration(&angled_samples(), 0.25, 42).unwrap();
        let cfg = crate::config::Config {
            calibration: Some(cal.clone()),
            ..Default::default()
        };
        let back =
            crate::config::Config::from_toml(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back.calibration, Some(cal));
    }
}

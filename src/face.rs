//! Face landmarks and head orientation.
//!
//! MediaPipe's FaceLandmarker gives 478 landmarks plus, optionally, a rigid
//! face-to-camera transform. Pose comes from the transform when present and
//! from a landmark-plane fallback otherwise. Either way the result is the
//! head's orientation *relative to the line from the face to the camera*, so
//! moving sideways in the frame does not look like turning the head.
//!
//! Convention (camera space, right-handed): +X image right, +Y up, +Z towards
//! the camera. `yaw > 0`: nose turned towards image right. `pitch > 0`: up.

use std::num::NonZeroU32;
use std::time::Instant;

use anyhow::{Context, Result};
use mediapipe::{
    Confidence, FaceLandmarker, Image, ModelSource, NormalizedLandmark, Size, Timestamp,
    Transform4x4,
};
use serde::Serialize;

use crate::capture::Frame;
use crate::config::FaceConfig;

/// Head orientation in degrees, relative to facing the camera.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Default)]
pub struct HeadPose {
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PoseMethod {
    TransformMatrix,
    Landmarks,
}

/// What one camera saw of the user's face in one frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct FaceObservation {
    pub pose: HeadPose,
    /// Inter-ocular distance as a fraction of frame width. Small values mean
    /// a distant (noisier) face.
    pub eye_span: f32,
    pub method: PoseMethod,
}

/// Landmark indices in MediaPipe's canonical face mesh.
pub mod lm {
    pub const NOSE_TIP: usize = 1;
    pub const FOREHEAD: usize = 10;
    /// Subject's right eye, outer corner (image left on an unmirrored camera).
    pub const RIGHT_EYE_OUTER: usize = 33;
    pub const CHIN: usize = 152;
    /// Subject's left eye, outer corner.
    pub const LEFT_EYE_OUTER: usize = 263;
}

/// Assumed horizontal field of view for the landmark fallback's
/// off-centre correction. Typical webcams are 65-78 degrees.
const ASSUMED_HFOV_DEG: f32 = 70.0;

type V3 = [f32; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn norm(a: V3) -> f32 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

fn yaw_pitch_of(v: V3) -> (f32, f32) {
    let yaw = v[0].atan2(v[2]);
    let pitch = v[1].atan2(v[0].hypot(v[2]));
    (yaw.to_degrees(), pitch.to_degrees())
}

fn wrap_deg(a: f32) -> f32 {
    let mut a = a % 360.0;
    if a > 180.0 {
        a -= 360.0;
    } else if a < -180.0 {
        a += 360.0;
    }
    a
}

/// Orientation of `forward` (the face normal) relative to `to_camera` (the
/// direction from the face towards the camera).
pub fn relative_pose(forward: V3, to_camera: V3, roll: f32) -> HeadPose {
    let (fy, fp) = yaw_pitch_of(forward);
    let (cy, cp) = yaw_pitch_of(to_camera);
    HeadPose {
        yaw: wrap_deg(fy - cy),
        pitch: fp - cp,
        roll,
    }
}

/// Pose from MediaPipe's facial transformation matrix (canonical face model
/// to camera space; the canonical face looks down +Z).
pub fn pose_from_transform(m: &Transform4x4) -> Option<HeadPose> {
    let forward = [m.get(0, 2), m.get(1, 2), m.get(2, 2)];
    let right = [m.get(0, 0), m.get(1, 0), m.get(2, 0)];
    let t = [m.get(0, 3), m.get(1, 3), m.get(2, 3)];
    if norm(forward) < 1e-6 || norm(t) < 1e-6 || !forward.iter().all(|v| v.is_finite()) {
        return None;
    }
    let to_camera = [-t[0], -t[1], -t[2]];
    let roll = right[1].atan2(right[0]).to_degrees();
    Some(relative_pose(forward, to_camera, roll))
}

/// Pose from the plane spanned by the eye corners and forehead-chin axis.
///
/// `aspect` is width / height of the analysed image: MediaPipe normalizes x
/// and y separately, and z is on the x scale.
pub fn pose_from_landmarks(points: &[[f32; 3]], aspect: f32) -> Option<HeadPose> {
    let need = lm::LEFT_EYE_OUTER.max(lm::CHIN) + 1;
    if points.len() < need {
        return None;
    }
    // Normalized image coords -> camera space in units of image height.
    let p = |i: usize| -> V3 {
        let [x, y, z] = points[i];
        [x * aspect, -y, -z * aspect]
    };
    let across = sub(p(lm::LEFT_EYE_OUTER), p(lm::RIGHT_EYE_OUTER));
    let up = sub(p(lm::FOREHEAD), p(lm::CHIN));
    let forward = cross(across, up);
    if norm(forward) < 1e-9 {
        return None;
    }
    let roll = across[1].atan2(across[0]).to_degrees();

    // Off-centre correction under a pinhole model with an assumed FOV.
    let [nx, ny, _] = points[lm::NOSE_TIP];
    let f = 0.5 * aspect / (ASSUMED_HFOV_DEG.to_radians() / 2.0).tan();
    let to_camera = [-(nx - 0.5) * aspect, (ny - 0.5), f];
    Some(relative_pose(forward, to_camera, roll))
}

fn eye_span(points: &[[f32; 3]]) -> f32 {
    let a = points[lm::LEFT_EYE_OUTER];
    let b = points[lm::RIGHT_EYE_OUTER];
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn to_points(face: &[NormalizedLandmark]) -> Vec<[f32; 3]> {
    face.iter()
        .map(|l| [l.point.x(), l.point.y(), l.point.z()])
        .collect()
}

/// One MediaPipe landmarker per camera (video mode keeps per-stream tracking
/// state, and needs monotonically increasing timestamps).
pub struct FaceTracker {
    landmarker: FaceLandmarker,
    origin: Instant,
    last_ts_ms: i64,
}

impl FaceTracker {
    pub fn new(cfg: &FaceConfig) -> Result<Self> {
        let model = cfg.resolve_model_path()?;
        let landmarker = FaceLandmarker::builder(ModelSource::path(model))
            .num_faces(NonZeroU32::new(2).expect("2 is non-zero"))
            .min_face_detection_confidence(Confidence::new(cfg.min_detection_confidence)?)
            .min_face_presence_confidence(Confidence::new(cfg.min_presence_confidence)?)
            .output_transformation_matrixes(true)
            .build_for_video()
            .context("creating MediaPipe FaceLandmarker")?;
        Ok(Self {
            landmarker,
            origin: Instant::now(),
            last_ts_ms: -1,
        })
    }

    /// `Ok(None)` means no face in this frame.
    pub fn analyze(&mut self, frame: &Frame) -> Result<Option<FaceObservation>> {
        let size = Size {
            width: frame.width,
            height: frame.height,
        };
        let image = Image::from_rgb(size, &frame.rgb)?;
        let ts = (frame
            .captured_at
            .saturating_duration_since(self.origin)
            .as_millis() as i64)
            .max(self.last_ts_ms + 1);
        self.last_ts_ms = ts;
        let result = self
            .landmarker
            .detect_for_video(&image, Timestamp::from_millis(ts))?;

        // With two faces (someone walking behind), take the nearest one.
        let Some((idx, points)) = result
            .landmarks
            .iter()
            .map(|f| to_points(f))
            .enumerate()
            .filter(|(_, p)| p.len() > lm::LEFT_EYE_OUTER)
            .max_by(|(_, a), (_, b)| eye_span(a).total_cmp(&eye_span(b)))
        else {
            return Ok(None);
        };

        let aspect = frame.width as f32 / frame.height.max(1) as f32;
        let from_matrix = result
            .transformation_matrixes
            .get(idx)
            .and_then(pose_from_transform);
        let (pose, method) = match from_matrix {
            Some(p) => (p, PoseMethod::TransformMatrix),
            None => match pose_from_landmarks(&points, aspect) {
                Some(p) => (p, PoseMethod::Landmarks),
                None => return Ok(None),
            },
        };
        Ok(Some(FaceObservation {
            pose,
            eye_span: eye_span(&points),
            method,
        }))
    }

    /// Both estimates side by side, for `autocam watch`.
    pub fn analyze_both(
        &mut self,
        frame: &Frame,
    ) -> Result<Option<(Option<HeadPose>, Option<HeadPose>)>> {
        let size = Size {
            width: frame.width,
            height: frame.height,
        };
        let image = Image::from_rgb(size, &frame.rgb)?;
        let ts = (frame
            .captured_at
            .saturating_duration_since(self.origin)
            .as_millis() as i64)
            .max(self.last_ts_ms + 1);
        self.last_ts_ms = ts;
        let result = self
            .landmarker
            .detect_for_video(&image, Timestamp::from_millis(ts))?;
        let Some(face) = result.landmarks.first() else {
            return Ok(None);
        };
        let aspect = frame.width as f32 / frame.height.max(1) as f32;
        Ok(Some((
            result
                .transformation_matrixes
                .first()
                .and_then(pose_from_transform),
            pose_from_landmarks(&to_points(face), aspect),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal synthetic face: eye corners, forehead, chin and nose tip in a
    /// canonical frame (face looking down +Z), rotated by yaw then pitch and
    /// projected orthographically into normalized image coordinates, placed
    /// so the nose tip lands on `nose_at`.
    fn synthetic_face(yaw_deg: f32, pitch_deg: f32, nose_at: (f32, f32)) -> Vec<[f32; 3]> {
        let mut pts = vec![[0.0f32; 3]; 478];
        let canon: [(usize, V3); 5] = [
            (lm::RIGHT_EYE_OUTER, [-0.05, 0.02, -0.02]),
            (lm::LEFT_EYE_OUTER, [0.05, 0.02, -0.02]),
            (lm::FOREHEAD, [0.0, 0.09, -0.01]),
            (lm::CHIN, [0.0, -0.09, -0.01]),
            (lm::NOSE_TIP, [0.0, -0.01, 0.04]),
        ];
        let (sy, cy) = yaw_deg.to_radians().sin_cos();
        let (sp, cp) = pitch_deg.to_radians().sin_cos();
        for (i, [x, y, z]) in canon {
            // Pitch about X (positive tips the face normal up), then yaw about Y.
            let (y1, z1) = (y * cp + z * sp, -y * sp + z * cp);
            let (x2, z2) = (x * cy + z1 * sy, -x * sy + z1 * cy);
            // Camera space (Y up, Z to camera) -> normalized image (square image).
            pts[i] = [x2, -y1, -z2];
        }
        let [nx, ny, _] = pts[lm::NOSE_TIP];
        for i in [
            lm::RIGHT_EYE_OUTER,
            lm::LEFT_EYE_OUTER,
            lm::FOREHEAD,
            lm::CHIN,
            lm::NOSE_TIP,
        ] {
            pts[i][0] += nose_at.0 - nx;
            pts[i][1] += nose_at.1 - ny;
        }
        pts
    }

    #[test]
    fn landmarks_frontal_is_zero() {
        let p = pose_from_landmarks(&synthetic_face(0.0, 0.0, (0.5, 0.5)), 1.0).unwrap();
        assert!(p.yaw.abs() < 0.5, "{p:?}");
        assert!(p.pitch.abs() < 0.5, "{p:?}");
        assert!(p.roll.abs() < 0.5, "{p:?}");
    }

    #[test]
    fn landmarks_recover_yaw_and_pitch() {
        for yaw in [-40.0f32, -20.0, 15.0, 35.0] {
            let p = pose_from_landmarks(&synthetic_face(yaw, 0.0, (0.5, 0.5)), 1.0).unwrap();
            assert!((p.yaw - yaw).abs() < 1.0, "yaw {yaw}: {p:?}");
        }
        for pitch in [-30.0f32, -10.0, 20.0] {
            let p = pose_from_landmarks(&synthetic_face(0.0, pitch, (0.5, 0.5)), 1.0).unwrap();
            assert!((p.pitch - pitch).abs() < 1.0, "pitch {pitch}: {p:?}");
        }
    }

    #[test]
    fn off_centre_face_looking_at_camera_is_frontal() {
        // Face at the right edge of the frame, turned to face the camera:
        // relative yaw should be ~0 once the off-centre angle is removed.
        let aspect = 1.0f32;
        let f = 0.5 * aspect / (ASSUMED_HFOV_DEG.to_radians() / 2.0).tan();
        let off = (0.3f32 / f).atan().to_degrees(); // nose 0.3 right of centre
        let p = pose_from_landmarks(&synthetic_face(-off, 0.0, (0.8, 0.5)), aspect).unwrap();
        assert!(p.yaw.abs() < 0.5, "{p:?}");
        // Without the correction this face would read as turned by `off`.
        assert!(off > 20.0);
    }

    #[test]
    fn too_few_landmarks_is_none() {
        assert!(pose_from_landmarks(&[[0.0; 3]; 10], 1.0).is_none());
    }

    #[test]
    fn wrap_keeps_range() {
        assert_eq!(wrap_deg(190.0), -170.0);
        assert_eq!(wrap_deg(-190.0), 170.0);
        assert_eq!(wrap_deg(45.0), 45.0);
    }

    #[test]
    fn relative_pose_subtracts_camera_direction() {
        // Facing a camera that is 30 deg to the side is still "frontal".
        let to_cam = [30f32.to_radians().sin(), 0.0, 30f32.to_radians().cos()];
        let p = relative_pose(to_cam, to_cam, 0.0);
        assert!(p.yaw.abs() < 1e-3 && p.pitch.abs() < 1e-3);
    }
}

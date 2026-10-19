//! AutoCam: switch OBS between webcam scenes based on which monitor the user
//! is using (mouse) or facing (head pose).
//!
//! Module map:
//! - [`obs`]: OBS WebSocket transport (connect, reconnect, screenshots, scene switching)
//! - [`capture`]: frame acquisition abstraction and frame-health checks
//! - [`mouse`]: mouse-position detector (monitor under the cursor selects the camera)
//! - [`face`]: MediaPipe face landmarks and head-pose estimation
//! - [`scoring`]: calibration, per-camera scores and the per-tick decision
//! - [`switching`]: hysteresis / dwell / manual-override state machine
//! - [`config`]: local TOML configuration
//! - [`diagnostics`]: metrics, localhost status endpoint, resource sampling
//! - [`app`], [`spike`], [`calibrate`], [`simulate`]: the runnable commands

pub mod app;
pub mod calibrate;
pub mod capture;
pub mod config;
pub mod control;
pub mod diagnostics;
pub mod face;
pub mod monitors;
pub mod mouse;
pub mod obs;
pub mod scoring;
pub mod simulate;
pub mod spike;
pub mod switching;

use serde::{Deserialize, Serialize};

/// Most cameras AutoCam handles (hotkeys Ctrl+Alt+1..9).
pub const MAX_CAMERAS: usize = 9;

/// A camera, by its position in the configured camera list (0-based).
/// Names live in the config ([`config::Config::cam_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CamId(pub usize);

impl CamId {
    pub fn index(self) -> usize {
        self.0
    }

    /// 1-based number, as used by hotkeys and the console.
    pub fn number(self) -> usize {
        self.0 + 1
    }
}

impl std::fmt::Display for CamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "camera {}", self.number())
    }
}

/// One value per camera, indexed by [`CamId`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cams<T>(pub Vec<T>);

impl<T> Cams<T> {
    pub fn from_fn(n: usize, f: impl FnMut(usize) -> T) -> Self {
        Self((0..n).map(f).collect())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, cam: CamId) -> &T {
        &self.0[cam.0]
    }

    pub fn get_mut(&mut self, cam: CamId) -> &mut T {
        &mut self.0[cam.0]
    }

    pub fn ids(&self) -> impl Iterator<Item = CamId> + use<T> {
        (0..self.0.len()).map(CamId)
    }

    pub fn iter(&self) -> impl Iterator<Item = (CamId, &T)> {
        self.0.iter().enumerate().map(|(i, v)| (CamId(i), v))
    }

    pub fn map<U>(self, mut f: impl FnMut(CamId, T) -> U) -> Cams<U> {
        Cams(
            self.0
                .into_iter()
                .enumerate()
                .map(|(i, v)| f(CamId(i), v))
                .collect(),
        )
    }
}

impl<T: Clone> Cams<T> {
    pub fn filled(n: usize, value: T) -> Self {
        Self(vec![value; n])
    }
}

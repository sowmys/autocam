//! AutoCam: switch OBS between two webcam scenes based on which monitor the
//! user is facing.
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

/// One of the two cameras. Each camera sits on (and is mapped to) one monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CamId {
    Left,
    Right,
}

impl CamId {
    pub const ALL: [CamId; 2] = [CamId::Left, CamId::Right];

    pub fn other(self) -> CamId {
        match self {
            CamId::Left => CamId::Right,
            CamId::Right => CamId::Left,
        }
    }

    pub fn index(self) -> usize {
        match self {
            CamId::Left => 0,
            CamId::Right => 1,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            CamId::Left => "left",
            CamId::Right => "right",
        }
    }
}

impl std::fmt::Display for CamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for CamId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "left" | "l" => Ok(CamId::Left),
            "right" | "r" => Ok(CamId::Right),
            other => Err(format!("unknown camera `{other}` (expected left or right)")),
        }
    }
}

/// A pair of values indexed by [`CamId`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PerCam<T> {
    pub left: T,
    pub right: T,
}

impl<T> PerCam<T> {
    pub fn new(left: T, right: T) -> Self {
        Self { left, right }
    }

    pub fn get(&self, cam: CamId) -> &T {
        match cam {
            CamId::Left => &self.left,
            CamId::Right => &self.right,
        }
    }

    pub fn get_mut(&mut self, cam: CamId) -> &mut T {
        match cam {
            CamId::Left => &mut self.left,
            CamId::Right => &mut self.right,
        }
    }

    pub fn map<U>(self, mut f: impl FnMut(CamId, T) -> U) -> PerCam<U> {
        PerCam {
            left: f(CamId::Left, self.left),
            right: f(CamId::Right, self.right),
        }
    }
}

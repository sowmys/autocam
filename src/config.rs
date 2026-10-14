//! Local configuration, stored as TOML in `%APPDATA%\AutoCam\config.toml`.
//!
//! Every field has a default, so a partial file (or none at all) is valid.
//! The OBS password may also come from `AUTOCAM_OBS_PASSWORD`, which wins over
//! the file. It is never logged: `ObsConfig` has a redacting `Debug`.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::scoring::Calibration;
use crate::switching::Mode;
use crate::{CamId, PerCam};

pub const PASSWORD_ENV: &str = "AUTOCAM_OBS_PASSWORD";

/// What decides which camera is live.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DetectorKind {
    /// The monitor under the mouse cursor (see [`MouseConfig`]).
    #[default]
    Mouse,
    /// Head orientation from MediaPipe face landmarks.
    Face,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub detector: DetectorKind,
    pub obs: ObsConfig,
    pub cameras: PerCam<CameraConfig>,
    pub capture: CaptureConfig,
    pub face: FaceConfig,
    pub switching: SwitchingConfig,
    pub mouse: MouseConfig,
    pub diagnostics: DiagnosticsConfig,
    /// Written by `autocam calibrate`. Without it, the defaults assume each
    /// camera sits on its monitor and points straight at the user.
    pub calibration: Option<Calibration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            detector: DetectorKind::default(),
            mouse: MouseConfig::default(),
            obs: ObsConfig::default(),
            cameras: PerCam::new(
                CameraConfig {
                    source: "AutoCam Left".into(),
                    scene: "LeftCam".into(),
                },
                CameraConfig {
                    source: "AutoCam Right".into(),
                    scene: "RightCam".into(),
                },
            ),
            capture: CaptureConfig::default(),
            face: FaceConfig::default(),
            switching: SwitchingConfig::default(),
            diagnostics: DiagnosticsConfig::default(),
            calibration: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ObsConfig {
    pub host: String,
    pub port: u16,
    /// Prefer the `AUTOCAM_OBS_PASSWORD` environment variable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    pub reconnect_initial_ms: u64,
    pub reconnect_max_ms: u64,
    /// Per-request timeout for screenshots and scene changes.
    pub request_timeout_ms: u64,
}

impl Default for ObsConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 4455,
            password: None,
            reconnect_initial_ms: 500,
            reconnect_max_ms: 5_000,
            request_timeout_ms: 2_000,
        }
    }
}

impl std::fmt::Debug for ObsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObsConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("reconnect_initial_ms", &self.reconnect_initial_ms)
            .field("reconnect_max_ms", &self.reconnect_max_ms)
            .field("request_timeout_ms", &self.request_timeout_ms)
            .finish()
    }
}

impl ObsConfig {
    /// The effective password: environment first, then the file.
    pub fn resolved_password(&self) -> Option<String> {
        std::env::var(PASSWORD_ENV)
            .ok()
            .filter(|p| !p.is_empty())
            .or_else(|| self.password.clone().filter(|p| !p.is_empty()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CameraConfig {
    /// OBS input (or scene) to sample with GetSourceScreenshot.
    pub source: String,
    /// OBS scene to put on Program when this camera is selected.
    pub scene: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CaptureConfig {
    /// Samples per second, per camera.
    pub fps: f32,
    /// Size OBS scales the screenshot to before encoding. Keeps the payload
    /// and decode cost small; MediaPipe crops the face itself.
    pub width: u32,
    pub height: u32,
    /// JPEG quality for the screenshot (OBS-side encode), 1-100.
    pub jpeg_quality: i32,
    /// After this many byte-identical frames in a row the camera is stale.
    pub stale_after_identical: u32,
    /// Mean luma below this is treated as "no signal" (lens cap, dead device).
    pub black_luma_threshold: f32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            fps: 4.0,
            width: 480,
            height: 270,
            jpeg_quality: 80,
            stale_after_identical: 8,
            black_luma_threshold: 6.0,
        }
    }
}

impl CaptureConfig {
    pub fn period(&self) -> Duration {
        Duration::from_secs_f32(1.0 / self.fps.clamp(0.5, 30.0))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FaceConfig {
    /// Path to MediaPipe's `face_landmarker.task`. Relative paths are tried
    /// against the working directory, then next to the executable.
    pub model_path: PathBuf,
    pub min_detection_confidence: f32,
    pub min_presence_confidence: f32,
}

impl Default for FaceConfig {
    fn default() -> Self {
        Self {
            model_path: PathBuf::from("models/face_landmarker.task"),
            min_detection_confidence: 0.5,
            min_presence_confidence: 0.5,
        }
    }
}

impl FaceConfig {
    pub fn resolve_model_path(&self) -> Result<PathBuf> {
        let p = &self.model_path;
        if p.is_absolute() {
            return check_exists(p.clone());
        }
        let mut candidates = vec![std::env::current_dir()?.join(p)];
        if let Some(dir) = std::env::current_exe()?.parent() {
            candidates.push(dir.join(p));
            // `target/release/autocam.exe` -> repo root.
            if let Some(repo) = dir.parent().and_then(Path::parent) {
                candidates.push(repo.join(p));
            }
        }
        candidates
            .into_iter()
            .find(|c| c.exists())
            .with_context(|| {
                format!(
                    "face model `{}` not found; run scripts\\fetch-model.ps1",
                    p.display()
                )
            })
    }
}

fn check_exists(p: PathBuf) -> Result<PathBuf> {
    if p.exists() {
        Ok(p)
    } else {
        bail!(
            "face model `{}` not found; run scripts\\fetch-model.ps1",
            p.display()
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SwitchingConfig {
    /// Startup mode.
    pub mode: Mode,
    /// The opposing camera must win every tick for this long.
    pub dwell_ms: u64,
    /// Minimum time between two automatic switches.
    pub min_switch_interval_ms: u64,
    /// Winner's score must beat the loser's by at least this much (0..1).
    pub confidence_margin: f32,
    /// Winner's score must be at least this (0..1).
    pub min_score: f32,
    /// Face-tracking dropouts shorter than this do not restart the dwell
    /// (they never cause a switch either). 0 = strict.
    pub max_gap_ms: u64,
    /// Minimum opposing verdicts within a dwell.
    pub min_votes: u32,
}

impl Default for SwitchingConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            dwell_ms: 800,
            min_switch_interval_ms: 1500,
            confidence_margin: 0.25,
            min_score: 0.45,
            max_gap_ms: 500,
            min_votes: 3,
        }
    }
}

impl SwitchingConfig {
    pub fn params(&self) -> crate::switching::SwitchParams {
        crate::switching::SwitchParams {
            dwell: Duration::from_millis(self.dwell_ms),
            min_interval: Duration::from_millis(self.min_switch_interval_ms),
            max_gap: Duration::from_millis(self.max_gap_ms),
            min_votes: self.min_votes.max(1),
        }
    }

    pub fn decision(&self) -> crate::scoring::DecisionParams {
        crate::scoring::DecisionParams {
            min_score: self.min_score,
            margin: self.confidence_margin,
        }
    }
}

/// Mouse detector settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MouseConfig {
    /// Displays that select the left camera, by device name (`DISPLAY2` or
    /// `\\.\DISPLAY2`; see `autocam monitors`). Empty on both sides with
    /// exactly two displays: leftmost -> left camera, rightmost -> right.
    pub left_monitors: Vec<String>,
    /// Displays that select the right camera.
    pub right_monitors: Vec<String>,
    /// Cursor polls per second.
    pub poll_hz: f32,
    /// Cursor must stay on the other camera's display this long.
    pub dwell_ms: u64,
    /// Camera health screenshots per second (never switch to a black or
    /// frozen camera). 0 disables health checks.
    pub health_check_fps: f32,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            left_monitors: Vec::new(),
            right_monitors: Vec::new(),
            poll_hz: 10.0,
            dwell_ms: 300,
            health_check_fps: 1.0,
        }
    }
}

impl MouseConfig {
    pub fn poll_period(&self) -> Duration {
        Duration::from_secs_f32(1.0 / self.poll_hz.clamp(1.0, 60.0))
    }

    /// Mouse position is exact, so one vote suffices; the minimum interval
    /// and the missing-evidence gap come from `[switching]`.
    pub fn switch_params(&self, s: &SwitchingConfig) -> crate::switching::SwitchParams {
        crate::switching::SwitchParams {
            dwell: Duration::from_millis(self.dwell_ms),
            min_votes: 1,
            ..s.params()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DiagnosticsConfig {
    /// Serve `/status` and the control endpoints. Loopback only.
    pub http_enabled: bool,
    pub http_bind: SocketAddr,
    /// Global hotkeys: Ctrl+Alt+Left/Right select, Ctrl+Alt+A toggles auto,
    /// Ctrl+Alt+Shift+Left/Right label ground truth for measurements.
    pub hotkeys: bool,
    /// Seconds between summary log lines.
    pub summary_interval_s: u64,
    /// A switch reversed within this window counts as a likely false switch.
    pub reversal_window_ms: u64,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            http_enabled: true,
            http_bind: SocketAddr::from(([127, 0, 0, 1], 8787)),
            hotkeys: true,
            summary_interval_s: 10,
            reversal_window_ms: 5_000,
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("AutoCam").join("config.toml")
    }

    /// Loads the file, or defaults if it does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        let cfg = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Config>(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Writes atomically (temp file + rename) so a crash cannot truncate it.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if !is_loopback_host(&self.obs.host) {
            bail!(
                "obs.host `{}` is not a loopback address; AutoCam only talks to a local OBS",
                self.obs.host
            );
        }
        if !self.diagnostics.http_bind.ip().is_loopback() {
            bail!(
                "diagnostics.http_bind `{}` must be a loopback address",
                self.diagnostics.http_bind
            );
        }
        if self.cameras.left.scene == self.cameras.right.scene {
            bail!("cameras.left.scene and cameras.right.scene must differ");
        }
        let s = &self.switching;
        if !(0.0..=1.0).contains(&s.confidence_margin) || !(0.0..=1.0).contains(&s.min_score) {
            bail!("switching.confidence_margin and min_score must be within 0..1");
        }
        if !(0.5..=30.0).contains(&self.capture.fps) {
            bail!("capture.fps must be within 0.5..30");
        }
        if !(1.0..=60.0).contains(&self.mouse.poll_hz) {
            bail!("mouse.poll_hz must be within 1..60");
        }
        if !(0.0..=10.0).contains(&self.mouse.health_check_fps) {
            bail!("mouse.health_check_fps must be within 0..10");
        }
        Ok(())
    }

    pub fn camera(&self, cam: CamId) -> &CameraConfig {
        self.cameras.get(cam)
    }

    /// Which camera a scene name belongs to, if any.
    pub fn cam_for_scene(&self, scene: &str) -> Option<CamId> {
        CamId::ALL
            .into_iter()
            .find(|c| self.cameras.get(*c).scene == scene)
    }
}

pub fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.trim_matches(|c| c == '[' || c == ']')
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate_and_roundtrip() {
        let cfg = Config::default();
        cfg.validate().unwrap();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn partial_file_fills_defaults() {
        let cfg: Config = toml::from_str("[obs]\npassword = \"x\"\n").unwrap();
        assert_eq!(cfg.obs.port, 4455);
        assert_eq!(cfg.switching.dwell_ms, 800);
        assert_eq!(cfg.obs.password.as_deref(), Some("x"));
    }

    #[test]
    fn rejects_non_loopback_endpoints() {
        let mut cfg = Config::default();
        cfg.obs.host = "192.168.1.5".into();
        assert!(cfg.validate().is_err());

        let mut cfg = Config::default();
        cfg.diagnostics.http_bind = "0.0.0.0:8787".parse().unwrap();
        assert!(cfg.validate().is_err());

        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
    }

    #[test]
    fn debug_redacts_password() {
        let mut cfg = Config::default();
        cfg.obs.password = Some("hunter2".into());
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("hunter2"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn save_and_load() {
        let dir = std::env::temp_dir().join(format!("autocam-cfg-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.switching.dwell_ms = 900;
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
        std::fs::remove_dir_all(dir).ok();
    }
}

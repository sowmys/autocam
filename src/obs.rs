//! OBS WebSocket transport (obs-websocket 5.x via `obws`).
//!
//! - Loopback only (enforced by config validation), password-authenticated.
//! - Every request has a timeout, so a hung OBS cannot stall the tick loop.
//! - [`connect_with_backoff`] retries forever with capped exponential backoff.
//! - Screenshots are requested scaled and JPEG-encoded by OBS, decoded in
//!   memory, and never written to disk or logged.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use obws::Client;
use obws::requests::inputs::{Create as CreateInput, InputId, SetSettings};
use obws::requests::scene_items::CreateSceneItem;
use obws::requests::sources::{SourceId, TakeScreenshot};
use serde_json::json;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::CamId;
use crate::capture::{CaptureError, Frame, decode_data_uri};
use crate::config::{CameraConfig, CaptureConfig, Config, ObsConfig};

pub struct Obs {
    client: Client,
    timeout: Duration,
}

impl Obs {
    pub async fn connect(cfg: &ObsConfig) -> Result<Self> {
        let password = cfg.resolved_password();
        if password.is_none() {
            warn!(
                "no OBS WebSocket password configured; set {} or obs.password",
                crate::config::PASSWORD_ENV
            );
        }
        let client = tokio::time::timeout(
            Duration::from_secs(5),
            Client::connect(&cfg.host, cfg.port, password.as_deref()),
        )
        .await
        .map_err(|_| anyhow!("timed out connecting to OBS at {}:{}", cfg.host, cfg.port))?
        .map_err(|e| {
            // obws errors never include the password.
            anyhow!("connecting to OBS at {}:{}: {e}", cfg.host, cfg.port)
        })?;
        Ok(Self {
            client,
            timeout: Duration::from_millis(cfg.request_timeout_ms),
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    async fn timed<T>(
        &self,
        fut: impl Future<Output = obws::error::Result<T>>,
    ) -> Result<T, CaptureError> {
        match tokio::time::timeout(self.timeout, fut).await {
            Err(_) => Err(CaptureError::Timeout),
            Ok(Err(obws::error::Error::Disconnected)) => Err(CaptureError::NotConnected),
            Ok(Err(e)) => Err(CaptureError::Obs(e.to_string())),
            Ok(Ok(v)) => Ok(v),
        }
    }

    /// GetSourceScreenshot, scaled by OBS, decoded to RGB in memory.
    pub async fn screenshot(
        &self,
        cam: CamId,
        source: &str,
        cap: &CaptureConfig,
    ) -> Result<Frame, CaptureError> {
        let req = TakeScreenshot {
            source: SourceId::Name(source),
            format: "jpg",
            width: Some(cap.width),
            height: Some(cap.height),
            compression_quality: Some(cap.jpeg_quality.clamp(1, 100)),
        };
        let uri = self
            .timed(self.client.sources().take_screenshot(req))
            .await?;
        let at = Instant::now();
        // Decoding a 480x270 JPEG takes ~1 ms; not worth a blocking thread.
        decode_data_uri(cam, &uri, at)
    }

    pub async fn set_program_scene(&self, scene: &str) -> Result<(), CaptureError> {
        self.timed(self.client.scenes().set_current_program_scene(scene))
            .await
    }

    pub async fn program_scene(&self) -> Result<String, CaptureError> {
        self.timed(self.client.scenes().current_program_scene())
            .await
            .map(|s| s.id.name)
    }

    /// (active on Program, showing anywhere) for a source.
    pub async fn source_active(&self, source: &str) -> Result<(bool, bool), CaptureError> {
        self.timed(self.client.sources().active(SourceId::Name(source)))
            .await
            .map(|a| (a.active, a.showing))
    }

    pub async fn version(&self) -> Result<String, CaptureError> {
        self.timed(self.client.general().version()).await.map(|v| {
            format!(
                "OBS {} / obs-websocket {}",
                v.obs_studio_version, v.obs_web_socket_version
            )
        })
    }

    pub async fn scene_names(&self) -> Result<Vec<String>, CaptureError> {
        self.timed(self.client.scenes().list())
            .await
            .map(|s| s.scenes.into_iter().map(|s| s.id.name).collect())
    }

    pub async fn input_names(&self) -> Result<Vec<String>, CaptureError> {
        self.timed(self.client.inputs().list(None))
            .await
            .map(|v| v.into_iter().map(|i| i.id.name).collect())
    }

    /// Checks the configured scenes and sources exist. Returns human-readable
    /// problems (empty = OK).
    pub async fn check_setup(&self, cfg: &Config) -> Result<Vec<String>> {
        let scenes = self.scene_names().await?;
        let inputs = self.input_names().await?;
        let mut problems = Vec::new();
        for c in &cfg.cameras {
            if !scenes.contains(&c.scene) {
                problems.push(format!(
                    "camera `{}`: scene `{}` not found in OBS",
                    c.name, c.scene
                ));
            }
            if !inputs.contains(&c.source) && !scenes.contains(&c.source) {
                problems.push(format!(
                    "camera `{}`: source `{}` not found in OBS (inputs or scenes)",
                    c.name, c.source
                ));
            }
        }
        if problems.is_empty() && !self.keepalive_ok(cfg).await.unwrap_or(false) {
            // Not a problem on tested webcams (the spike shows fresh frames
            // without it); the stale-frame detector covers devices where it is.
            info!("camera keepalive items not present (optional; `autocam obs-setup` adds them)");
        }
        Ok(problems)
    }

    /// Lists video capture devices OBS can open, as (display name, device id).
    pub async fn list_video_devices(&self, probe_input: &str) -> Result<Vec<(String, String)>> {
        let items = self
            .client
            .inputs()
            .properties_list_property_items(InputId::Name(probe_input), "video_device_id")
            .await
            .context("listing video devices")?;
        Ok(items
            .into_iter()
            .filter_map(|i| i.value.as_str().map(|v| (i.name.clone(), v.to_owned())))
            .filter(|(_, id)| !id.is_empty())
            .collect())
    }

    /// Creates (or updates) one scene and one camera input per camera.
    ///
    /// `assignments` are `(camera name, device)` in camera order; the device
    /// is a substring matched against OBS's device names. The camera list in
    /// `cfg` is replaced by these cameras: existing cameras keep their OBS
    /// names and display mapping, new ones get `AutoCam <Name>` / `<Name>Cam`.
    /// Then sets the canvas and camera resolution, fits the cameras to the
    /// frame, and adds the keepalive items.
    pub async fn setup_scenes(
        &self,
        cfg: &mut Config,
        assignments: &[(String, String)],
        resolution: (u32, u32),
    ) -> Result<()> {
        if assignments.len() < 2 {
            bail!("give at least two cameras, e.g. --camera left=dell --camera right=logitech");
        }
        cfg.cameras = assignments
            .iter()
            .map(|(name, _)| {
                cfg.cameras
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(name))
                    .cloned()
                    .unwrap_or_else(|| CameraConfig::named(&name.to_ascii_lowercase()))
            })
            .collect();
        cfg.validate()?;

        let scenes = self.scene_names().await?;
        for c in &cfg.cameras {
            if !scenes.contains(&c.scene) {
                self.client.scenes().create(&c.scene).await?;
                info!("created scene `{}`", c.scene);
            }
        }

        // Any camera input can list the devices; make sure the first exists.
        let mut inputs = self.input_names().await?;
        let first = cfg.cameras[0].clone();
        if !inputs.contains(&first.source) {
            self.create_camera_input(&first.scene, &first.source)
                .await?;
            inputs.push(first.source.clone());
        }
        let devices = self.list_video_devices(&first.source).await?;
        if devices.is_empty() {
            bail!("OBS reports no video capture devices");
        }
        let available = || {
            devices
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut picked: Vec<(String, String)> = Vec::new();
        for (name, want) in assignments {
            let (dname, id) = devices
                .iter()
                .find(|(n, _)| n.to_lowercase().contains(&want.to_lowercase()))
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "camera `{name}`: no video device matching `{want}`; available: {}",
                        available()
                    )
                })?;
            if let Some((other, _)) = picked.iter().find(|(_, i)| *i == id) {
                bail!(
                    "cameras `{other}` and `{name}` both matched `{dname}`; use a more specific name"
                );
            }
            picked.push((name.clone(), id));
        }

        for (c, (_, id)) in cfg.cameras.clone().iter().zip(&picked) {
            if !inputs.contains(&c.source) {
                self.create_camera_input(&c.scene, &c.source).await?;
                inputs.push(c.source.clone());
            } else {
                self.ensure_in_scene(&c.source, &c.scene).await?;
            }
            self.configure_camera(&c.source, id).await?;
            let dname = devices
                .iter()
                .find(|(_, i)| i == id)
                .map_or("?", |(n, _)| n);
            info!("camera `{}`: `{}` -> {dname}", c.name, c.source);
        }

        for c in &cfg.cameras {
            match self.set_camera_resolution(&c.source, resolution).await {
                Ok(r) => info!("`{}` resolution {r}", c.source),
                Err(e) => {
                    warn!(
                        "could not set `{}` resolution ({e:#}); keeping device default",
                        c.source
                    )
                }
            }
        }
        self.set_canvas(resolution.0, resolution.1).await?;
        // Let OBS re-open the cameras at the new resolution before fitting.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        self.fit_cameras_to_canvas(cfg).await?;
        for a in self.ensure_keepalive(cfg).await? {
            info!("keepalive: {a} (tiny, off-canvas, locked)");
        }
        Ok(())
    }

    /// Adds an existing input to a scene if it is not already there.
    async fn ensure_in_scene(&self, source: &str, scene: &str) -> Result<()> {
        let items = self.client.scene_items().list(scene.into()).await?;
        if !items.iter().any(|i| i.source_name == source) {
            self.client
                .scene_items()
                .create(CreateSceneItem {
                    scene: scene.into(),
                    source: SourceId::Name(source),
                    enabled: Some(true),
                })
                .await
                .with_context(|| format!("adding `{source}` to `{scene}`"))?;
            info!("added `{source}` to `{scene}`");
        }
        Ok(())
    }
    /// Sets OBS's canvas and output resolution (30 fps). OBS refuses this
    /// while an output is active, so the Virtual Camera is stopped and
    /// restarted around the change (a one-second blip in Teams).
    pub async fn set_canvas(&self, width: u32, height: u32) -> Result<()> {
        use obws::requests::config::SetVideoSettings;
        let v = self.client.config().video_settings().await?;
        if (v.base_width, v.base_height, v.output_width, v.output_height)
            == (width, height, width, height)
        {
            info!("OBS canvas already {width}x{height}");
            return Ok(());
        }
        let vcam_on = self.client.virtual_cam().status().await.unwrap_or(false);
        if vcam_on {
            info!("stopping the Virtual Camera to change the canvas");
            self.client.virtual_cam().stop().await?;
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        let result = self
            .client
            .config()
            .set_video_settings(SetVideoSettings {
                base_width: Some(width),
                base_height: Some(height),
                output_width: Some(width),
                output_height: Some(height),
                fps_numerator: Some(30),
                fps_denominator: Some(1),
            })
            .await;
        if vcam_on {
            self.client.virtual_cam().start().await?;
            info!("Virtual Camera restarted");
        }
        result.context(
            "changing the OBS canvas (stop any recording or streaming in OBS and try again)",
        )?;
        info!(
            "OBS canvas {}x{} -> {width}x{height} @ 30 fps",
            v.base_width, v.base_height
        );
        Ok(())
    }

    /// Asks a camera which resolutions it supports and selects `want`
    /// (e.g. 1920x1080) or, failing that, the largest 16:9 mode not above it,
    /// then the largest mode not above it. Returns the chosen resolution.
    pub async fn set_camera_resolution(&self, input: &str, want: (u32, u32)) -> Result<String> {
        // Custom resolution mode makes OBS populate the device's mode list.
        self.client
            .inputs()
            .set_settings(SetSettings {
                input: InputId::Name(input),
                settings: &json!({ "res_type": 1 }),
                overlay: Some(true),
            })
            .await?;
        let items = self
            .client
            .inputs()
            .properties_list_property_items(InputId::Name(input), "resolution")
            .await
            .with_context(|| format!("listing resolutions of `{input}`"))?;
        let modes: Vec<(u32, u32)> = items
            .iter()
            .filter_map(|i| i.value.as_str().and_then(parse_resolution))
            .collect();
        let chosen = pick_resolution(&modes, want)
            .with_context(|| format!("`{input}` reported no resolutions"))?;
        let res = format!("{}x{}", chosen.0, chosen.1);
        self.client
            .inputs()
            .set_settings(SetSettings {
                input: InputId::Name(input),
                settings: &json!({ "res_type": 1, "resolution": res }),
                overlay: Some(true),
            })
            .await?;
        Ok(res)
    }

    /// Makes each camera fill its own scene, like OBS's "Fit to screen":
    /// scaled to the canvas with the aspect ratio kept, centred. Without this
    /// a 1080p camera on a 4K canvas covers only the top-left quarter.
    pub async fn fit_cameras_to_canvas(&self, cfg: &Config) -> Result<()> {
        use obws::common::{Alignment, BoundsType};
        use obws::requests::scene_items::{Bounds, Position, SceneItemTransform, SetTransform};

        let video = self.client.config().video_settings().await?;
        let (w, h) = (video.base_width as f32, video.base_height as f32);
        info!(
            "OBS canvas {}x{}, output {}x{} @ {}/{} fps",
            video.base_width,
            video.base_height,
            video.output_width,
            video.output_height,
            video.fps_numerator,
            video.fps_denominator
        );
        for c in &cfg.cameras {
            let items = self
                .client
                .scene_items()
                .list(c.scene.as_str().into())
                .await?;
            let Some(item) = items.iter().find(|i| i.source_name == c.source) else {
                warn!(
                    "`{}` is not in scene `{}`; cannot fit it",
                    c.source, c.scene
                );
                continue;
            };
            self.client
                .scene_items()
                .set_transform(SetTransform {
                    scene: c.scene.as_str().into(),
                    item_id: item.id,
                    transform: SceneItemTransform {
                        position: Some(Position {
                            x: Some(0.0),
                            y: Some(0.0),
                        }),
                        rotation: Some(0.0),
                        alignment: Some(Alignment::LEFT | Alignment::TOP),
                        bounds: Some(Bounds {
                            r#type: Some(BoundsType::ScaleInner),
                            alignment: Some(Alignment::CENTER),
                            width: Some(w),
                            height: Some(h),
                        }),
                        ..Default::default()
                    },
                })
                .await
                .with_context(|| format!("fitting `{}` in `{}`", c.source, c.scene))?;
            info!(
                "`{}` fitted to the {w}x{h} canvas in `{}`",
                c.source, c.scene
            );
        }
        Ok(())
    }

    async fn create_camera_input(&self, scene: &str, name: &str) -> Result<()> {
        self.client
            .inputs()
            .create(CreateInput {
                scene: scene.into(),
                input: name,
                kind: "dshow_input",
                settings: Some(json!({ "deactivate_when_not_showing": false })),
                enabled: Some(true),
            })
            .await
            .with_context(|| format!("creating input `{name}`"))?;
        info!("created camera input `{name}` in `{scene}`");
        Ok(())
    }

    /// Selects the device and keeps it running when not on Program, which is
    /// what lets GetSourceScreenshot return fresh frames for the inactive camera.
    async fn configure_camera(&self, input: &str, device_id: &str) -> Result<()> {
        let settings = json!({
            "video_device_id": device_id,
            "deactivate_when_not_showing": false,
        });
        self.client
            .inputs()
            .set_settings(SetSettings {
                input: InputId::Name(input),
                settings: &settings,
                overlay: Some(true),
            })
            .await
            .with_context(|| format!("configuring `{input}`"))
    }

    /// Sets `deactivate_when_not_showing` on a camera input (spike negative control).
    pub async fn set_deactivate_when_hidden(&self, input: &str, on: bool) -> Result<()> {
        let settings = json!({ "deactivate_when_not_showing": on });
        self.client
            .inputs()
            .set_settings(SetSettings {
                input: InputId::Name(input),
                settings: &settings,
                overlay: Some(true),
            })
            .await
            .with_context(|| format!("configuring `{input}`"))
    }

    /// Makes sure each camera source is also rendered while *another*
    /// camera's scene is on Program, by adding it to every other camera's
    /// scene as a tiny, locked item far off the canvas (never visible in the
    /// output).
    ///
    /// Insurance, not a requirement: with real webcams (Dell WB7022, Intel
    /// IPU) and "Deactivate when not showing" off, the spike measured fresh
    /// frames for the non-Program camera with or without keepalive. It keeps
    /// both cameras `active` so drivers or plugins that throttle hidden
    /// sources cannot freeze the inactive camera.
    pub async fn ensure_keepalive(&self, cfg: &Config) -> Result<Vec<String>> {
        use obws::requests::scene_items::{
            Position, Scale, SceneItemTransform, SetLocked, SetTransform,
        };
        let mut added = Vec::new();
        for (source, scene) in keepalive_pairs(cfg) {
            let items = self
                .client
                .scene_items()
                .list(scene.as_str().into())
                .await?;
            // An existing item (e.g. left over from simulation mode, or added
            // by hand) is converted to a keepalive so it is never visible.
            let id = match items.iter().find(|i| i.source_name == source) {
                Some(item) => item.id,
                None => self
                    .client
                    .scene_items()
                    .create(CreateSceneItem {
                        scene: scene.as_str().into(),
                        source: SourceId::Name(&source),
                        enabled: Some(true),
                    })
                    .await
                    .with_context(|| format!("adding keepalive `{source}` to `{scene}`"))?,
            };
            self.client
                .scene_items()
                .set_transform(SetTransform {
                    scene: scene.as_str().into(),
                    item_id: id,
                    transform: SceneItemTransform {
                        position: Some(Position {
                            x: Some(-20_000.0),
                            y: Some(-20_000.0),
                        }),
                        scale: Some(Scale {
                            x: Some(0.01),
                            y: Some(0.01),
                        }),
                        ..Default::default()
                    },
                })
                .await?;
            self.client
                .scene_items()
                .set_locked(SetLocked {
                    scene: scene.as_str().into(),
                    item_id: id,
                    locked: true,
                })
                .await?;
            added.push(format!("`{source}` -> `{scene}`"));
        }
        Ok(added)
    }

    /// Enables or disables the keepalive items (spike A/B test only).
    /// Returns how many items were changed.
    pub async fn set_keepalive_enabled(&self, cfg: &Config, enabled: bool) -> Result<usize> {
        use obws::requests::scene_items::SetEnabled;
        let mut n = 0;
        for (source, scene) in keepalive_pairs(cfg) {
            let scene = scene.as_str();
            for item in self.client.scene_items().list(scene.into()).await? {
                if item.source_name == source {
                    self.client
                        .scene_items()
                        .set_enabled(SetEnabled {
                            scene: scene.into(),
                            item_id: item.id,
                            enabled,
                        })
                        .await?;
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// True if every camera source is present (enabled) in every other
    /// camera's scene.
    pub async fn keepalive_ok(&self, cfg: &Config) -> Result<bool> {
        for (source, scene) in keepalive_pairs(cfg) {
            let items = self
                .client
                .scene_items()
                .list(scene.as_str().into())
                .await?;
            let mut found = false;
            for item in items.iter().filter(|i| i.source_name == source) {
                if self
                    .client
                    .scene_items()
                    .enabled(scene.as_str().into(), item.id)
                    .await?
                {
                    found = true;
                    break;
                }
            }
            if !found {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Re-applies a camera input's settings, which makes OBS re-open the
    /// device. Some cameras (e.g. Intel IPU) fail to restart after a settings
    /// change and stay black until re-activated.
    pub async fn reactivate_camera(&self, input: &str) -> Result<()> {
        let current = self
            .client
            .inputs()
            .settings::<serde_json::Value>(InputId::Name(input))
            .await
            .with_context(|| format!("reading `{input}` settings"))?;
        let mut settings = current.settings;
        settings["deactivate_when_not_showing"] = json!(false);
        // Toggling `active` forces a full close/open cycle in win-dshow.
        for active in [false, true] {
            settings["active"] = json!(active);
            self.client
                .inputs()
                .set_settings(SetSettings {
                    input: InputId::Name(input),
                    settings: &settings,
                    overlay: Some(true),
                })
                .await?;
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        Ok(())
    }

    /// OBS event stream (program scene changes, shutdown).
    pub fn events(&self) -> Result<impl tokio_stream::Stream<Item = obws::events::Event> + use<>> {
        self.client.events().context("subscribing to OBS events")
    }
}

pub fn parse_resolution(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Exact match; else the largest 16:9 mode within `want`; else the largest
/// mode within `want`; else the smallest mode overall.
pub fn pick_resolution(modes: &[(u32, u32)], want: (u32, u32)) -> Option<(u32, u32)> {
    if modes.contains(&want) {
        return Some(want);
    }
    let area = |m: &(u32, u32)| u64::from(m.0) * u64::from(m.1);
    let fits = |m: &&(u32, u32)| m.0 <= want.0 && m.1 <= want.1;
    let wide = |m: &&(u32, u32)| u64::from(m.0) * 9 == u64::from(m.1) * 16;
    modes
        .iter()
        .filter(fits)
        .filter(wide)
        .max_by_key(|m| area(m))
        .or_else(|| modes.iter().filter(fits).max_by_key(|m| area(m)))
        .or_else(|| modes.iter().min_by_key(|m| area(m)))
        .copied()
}

/// (camera source, other camera's scene) for every ordered pair of cameras:
/// where each keepalive item lives.
pub fn keepalive_pairs(cfg: &Config) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for c in &cfg.cameras {
        for o in &cfg.cameras {
            if o.scene != c.scene {
                pairs.push((c.source.clone(), o.scene.clone()));
            }
        }
    }
    pairs
}

/// Connects, retrying with capped exponential backoff until it succeeds or
/// `shutdown` flips to true. Each failure is logged once per attempt.
pub async fn connect_with_backoff(
    cfg: &ObsConfig,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<Obs> {
    let mut delay = Duration::from_millis(cfg.reconnect_initial_ms.max(50));
    let max = Duration::from_millis(cfg.reconnect_max_ms.max(cfg.reconnect_initial_ms));
    let mut attempt = 0u32;
    loop {
        if *shutdown.borrow() {
            return None;
        }
        attempt += 1;
        match Obs::connect(cfg).await {
            Ok(obs) => {
                match obs.version().await {
                    Ok(v) => info!("connected to {v}"),
                    Err(e) => warn!("connected, but version query failed: {e}"),
                }
                return Some(obs);
            }
            Err(e) => {
                warn!("OBS connect attempt {attempt} failed: {e:#}; retrying in {delay:?}");
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.changed() => return None,
        }
        delay = (delay * 2).min(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resolutions() {
        assert_eq!(parse_resolution("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_resolution(" 640 x 480 "), Some((640, 480)));
        assert_eq!(parse_resolution("bogus"), None);
    }

    #[test]
    fn picks_exact_then_widescreen_then_largest() {
        let dell = [(640, 480), (1280, 720), (1920, 1080), (3840, 2160)];
        assert_eq!(pick_resolution(&dell, (1920, 1080)), Some((1920, 1080)));
        // No exact match: best 16:9 that fits, even if a 4:3 mode is larger.
        let cam = [(640, 360), (1280, 720), (1600, 1200), (2592, 1944)];
        assert_eq!(pick_resolution(&cam, (1920, 1080)), Some((1280, 720)));
        // No 16:9 that fits: largest that fits.
        let old = [(320, 240), (640, 480), (2048, 1536)];
        assert_eq!(pick_resolution(&old, (1920, 1080)), Some((640, 480)));
        // Nothing fits: smallest available.
        assert_eq!(
            pick_resolution(&[(3840, 2160)], (1280, 720)),
            Some((3840, 2160))
        );
        assert_eq!(pick_resolution(&[], (1920, 1080)), None);
    }
}

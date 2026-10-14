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
use crate::config::{CaptureConfig, Config, ObsConfig};

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
        for cam in CamId::ALL {
            let c = cfg.camera(cam);
            if !scenes.contains(&c.scene) {
                problems.push(format!("{cam}: scene `{}` not found in OBS", c.scene));
            }
            if !inputs.contains(&c.source) && !scenes.contains(&c.source) {
                problems.push(format!(
                    "{cam}: source `{}` not found in OBS (inputs or scenes)",
                    c.source
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

    /// Creates (or updates) the LeftCam/RightCam scenes and camera inputs.
    ///
    /// `left_device`/`right_device` are substrings matched against OBS's
    /// device names. Without a right device, the right scene re-uses the left
    /// camera input so the pipeline can be exercised with one physical camera.
    pub async fn setup_scenes(
        &self,
        cfg: &mut Config,
        left_device: &str,
        right_device: Option<&str>,
    ) -> Result<()> {
        let scenes = self.scene_names().await?;
        let inputs = self.input_names().await?;
        let simulate_second = right_device.is_none();

        for cam in CamId::ALL {
            let scene = cfg.camera(cam).scene.clone();
            if !scenes.contains(&scene) {
                self.client.scenes().create(&scene).await?;
                info!("created scene `{scene}`");
            }
        }

        // The left camera input always exists; create it in the left scene.
        let left = cfg.cameras.left.clone();
        if !inputs.contains(&left.source) {
            self.create_camera_input(&left.scene, &left.source).await?;
        }
        let devices = self.list_video_devices(&left.source).await?;
        if devices.is_empty() {
            bail!("OBS reports no video capture devices");
        }
        let pick = |want: &str| -> Result<(String, String)> {
            devices
                .iter()
                .find(|(name, _)| name.to_lowercase().contains(&want.to_lowercase()))
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "no video device matching `{want}`; available: {}",
                        devices
                            .iter()
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
        };
        let (lname, lid) = pick(left_device)?;
        self.configure_camera(&left.source, &lid).await?;
        info!("`{}` -> {lname}", left.source);

        if simulate_second {
            // Same physical camera shown in the right scene.
            let right_scene = cfg.cameras.right.scene.clone();
            let items = self
                .client
                .scene_items()
                .list(right_scene.as_str().into())
                .await?;
            if !items.iter().any(|i| i.source_name == left.source) {
                let id = self
                    .client
                    .scene_items()
                    .create(CreateSceneItem {
                        scene: right_scene.as_str().into(),
                        source: SourceId::Name(&left.source),
                        enabled: Some(true),
                    })
                    .await?;
                info!("added `{}` to `{right_scene}` (item {id})", left.source);
            }
            cfg.cameras.right.source = left.source.clone();
            warn!(
                "simulation: both cameras sample `{}`; calibration targets alone tell them apart",
                left.source
            );
        } else {
            if cfg.cameras.right.source == left.source {
                // Leaving simulation mode: the right camera gets its own input.
                cfg.cameras.right.source = Config::default().cameras.right.source;
            }
            let right = cfg.cameras.right.clone();
            if !inputs.contains(&right.source) {
                self.create_camera_input(&right.scene, &right.source)
                    .await?;
            }
            let (rname, rid) = pick(right_device.unwrap_or_default())?;
            if rid == lid {
                bail!("left and right must be different devices (both matched `{rname}`)");
            }
            self.configure_camera(&right.source, &rid).await?;
            info!("`{}` -> {rname}", right.source);
        }
        for a in self.ensure_keepalive(cfg).await? {
            info!("keepalive: added {a} (tiny, off-canvas, locked)");
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

    /// Makes sure each camera source is also rendered while the *other*
    /// camera's scene is on Program, by adding it to that scene as a tiny,
    /// locked item far off the canvas (never visible in the output).
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
        if cfg.cameras.left.source == cfg.cameras.right.source {
            // Simulation mode: the shared input is each scene's main view.
            return Ok(added);
        }
        for cam in CamId::ALL {
            let source = cfg.camera(cam).source.clone();
            let scene = cfg.camera(cam.other()).scene.clone();
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
        if cfg.cameras.left.source == cfg.cameras.right.source {
            return Ok(0);
        }
        let mut n = 0;
        for cam in CamId::ALL {
            let source = &cfg.camera(cam).source;
            let scene = cfg.camera(cam.other()).scene.as_str();
            for item in self.client.scene_items().list(scene.into()).await? {
                if &item.source_name == source {
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

    /// True if every camera source is present (enabled) in both camera scenes.
    pub async fn keepalive_ok(&self, cfg: &Config) -> Result<bool> {
        for cam in CamId::ALL {
            let source = &cfg.camera(cam).source;
            let scene = &cfg.camera(cam.other()).scene;
            let items = self
                .client
                .scene_items()
                .list(scene.as_str().into())
                .await?;
            let mut found = false;
            for item in items.iter().filter(|i| &i.source_name == source) {
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

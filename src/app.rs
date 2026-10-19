//! The `run` command: sampling pipeline, switching loop, reconnects, controls.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};

use crate::capture::{CaptureError, Frame, HealthTracker, SignalFault};
use crate::config::{Config, DetectorKind, FaceConfig};
use crate::control::{CONSOLE_HELP, CamRef, Control};
use crate::diagnostics::{self, EvalTracker, ResourceSampler, SharedStatus, Status};
use crate::face::{FaceObservation, FaceTracker};
use crate::mouse;
use crate::obs::{Obs, connect_with_backoff};
use crate::scoring::{self, Calibration, CamSignal};
use crate::switching::{Event, Mode, SwitchCause, SwitchCommand, Switcher};
use crate::{CamId, Cams};
use futures_util::future::join_all;

// ---------------------------------------------------------------------------
// Vision worker: one MediaPipe landmarker per camera on its own OS thread.
// ---------------------------------------------------------------------------

type VisionReply = oneshot::Sender<Result<(Option<FaceObservation>, f32)>>;

pub struct VisionWorker {
    tx: std::sync::mpsc::Sender<(Frame, VisionReply)>,
}

impl VisionWorker {
    /// `face: false` runs without MediaPipe: every frame reports "no face",
    /// so automatic switching always holds (manual-only / plumbing test).
    pub fn spawn(cam: CamId, cfg: &FaceConfig, face: bool) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<(Frame, VisionReply)>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let cfg = cfg.clone();
        std::thread::Builder::new()
            .name(format!("vision-{cam}"))
            .spawn(move || {
                let mut tracker = if face {
                    match FaceTracker::new(&cfg) {
                        Ok(t) => Some(t),
                        Err(e) => {
                            ready_tx.send(Err(e)).ok();
                            return;
                        }
                    }
                } else {
                    None
                };
                ready_tx.send(Ok(())).ok();
                while let Ok((frame, reply)) = rx.recv() {
                    let t = Instant::now();
                    let r = match tracker.as_mut() {
                        Some(tr) => tr.analyze(&frame),
                        None => Ok(None),
                    }
                    .map(|o| (o, t.elapsed().as_secs_f32() * 1000.0));
                    // The frame is dropped here: pixels never outlive analysis.
                    drop(frame);
                    reply.send(r).ok();
                }
            })
            .context("spawning vision thread")?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("vision thread for {cam} exited during start-up"))?
            .context(
                "MediaPipe face tracking unavailable. If the libmediapipe download is blocked on \
                 your network, run scripts\\fetch-mediapipe.ps1 from another network or set \
                 MEDIAPIPE_LIB; `autocam run --no-face` runs in manual-only mode meanwhile",
            )?;
        Ok(Self { tx })
    }

    pub async fn analyze(&self, frame: Frame) -> Result<(Option<FaceObservation>, f32)> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send((frame, reply))
            .map_err(|_| anyhow!("vision thread stopped"))?;
        rx.await
            .map_err(|_| anyhow!("vision thread dropped request"))?
    }
}

// ---------------------------------------------------------------------------
// Sampling pipeline: screenshot -> health -> landmarks -> score
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CamSample {
    pub signal: CamSignal,
    pub observation: Option<FaceObservation>,
    pub capture_ms: Option<f32>,
    pub inference_ms: Option<f32>,
    pub error: Option<CaptureError>,
}

pub struct Pipeline {
    workers: Cams<VisionWorker>,
    health: Cams<HealthTracker>,
}

impl Pipeline {
    pub fn new(cfg: &Config) -> Result<Self> {
        Self::with_face(cfg, true)
    }

    pub fn with_face(cfg: &Config, face: bool) -> Result<Self> {
        if face {
            info!("loading MediaPipe face landmarker (first run downloads libmediapipe ~34 MB)");
        } else if cfg.detector == DetectorKind::Face {
            warn!("face tracking disabled: automatic switching will hold; manual control only");
        }
        let workers = cfg
            .cam_ids()
            .map(|cam| VisionWorker::spawn(cam, &cfg.face, face))
            .collect::<Result<Vec<_>>>()?;
        if face && let Ok(lib) = mediapipe::loader::lib() {
            info!("libmediapipe: {:?}", lib.source);
        }
        Ok(Self {
            health: Cams::from_fn(workers.len(), |_| HealthTracker::default()),
            workers: Cams(workers),
        })
    }

    /// Screenshots every camera (concurrently), checks frame health, runs
    /// face analysis on usable frames (concurrently, one thread per camera)
    /// and scores them.
    pub async fn sample(&mut self, obs: &Obs, cfg: &Config, cal: &Calibration) -> Cams<CamSample> {
        let cap = &cfg.capture;
        let shots = join_all(cfg.cam_ids().map(|cam| async move {
            let t = Instant::now();
            let r = obs.screenshot(cam, &cfg.camera(cam).source, cap).await;
            (r, t.elapsed().as_secs_f32() * 1000.0)
        }))
        .await;

        enum Pre {
            Usable(Frame),
            Fault(SignalFault, Option<CaptureError>),
        }
        // Health check; only usable frames go to the vision threads.
        let mut cap_ms = Vec::with_capacity(shots.len());
        let pre: Vec<Pre> = shots
            .into_iter()
            .enumerate()
            .map(|(i, (res, ms))| {
                cap_ms.push(ms);
                match res {
                    Ok(frame) => match self.health.get_mut(CamId(i)).check(
                        &frame,
                        cap.stale_after_identical,
                        cap.black_luma_threshold,
                    ) {
                        Some(f) => Pre::Fault(f, None),
                        None => Pre::Usable(frame),
                    },
                    Err(e) => Pre::Fault(SignalFault::CaptureFailed, Some(e)),
                }
            })
            .collect();

        let workers = &self.workers;
        let outcomes = join_all(pre.into_iter().enumerate().map(|(i, p)| async move {
            match p {
                Pre::Usable(frame) => Ok(workers.get(CamId(i)).analyze(frame).await),
                Pre::Fault(f, e) => Err((f, e)),
            }
        }))
        .await;

        Cams(outcomes).map(|cam, outcome| {
            let cap_ms = cap_ms[cam.0];
            match outcome {
                Ok(Ok((obs, ms))) => CamSample {
                    signal: match &obs {
                        Some(o) => CamSignal::Face(scoring::score(&cal.camera(cam), o)),
                        None => CamSignal::NoFace,
                    },
                    observation: obs,
                    capture_ms: Some(cap_ms),
                    inference_ms: Some(ms),
                    error: None,
                },
                Ok(Err(e)) => {
                    warn!(
                        "camera `{}`: face analysis failed: {e:#}",
                        cfg.cam_name(cam)
                    );
                    CamSample {
                        signal: CamSignal::Fault {
                            fault: SignalFault::CaptureFailed,
                        },
                        observation: None,
                        capture_ms: Some(cap_ms),
                        inference_ms: None,
                        error: None,
                    }
                }
                Err((fault, error)) => CamSample {
                    signal: CamSignal::Fault { fault },
                    observation: None,
                    capture_ms: error.is_none().then_some(cap_ms),
                    inference_ms: None,
                    error,
                },
            }
        })
    }
}
// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

pub struct RunOptions {
    pub config_path: PathBuf,
    pub console: bool,
    /// Run without MediaPipe (manual-only).
    pub face: bool,
}

struct Runner {
    cfg: Config,
    opts: RunOptions,
    cal: Calibration,
    pipeline: Pipeline,
    switcher: Switcher,
    eval: EvalTracker,
    status: SharedStatus,
    last_auto: Option<(Instant, CamId, CamId)>,
    started: Instant,
    /// Present when `detector = "mouse"`.
    mouse: Option<MouseState>,
}

/// Mouse detector state: display layout, mapping and camera health.
struct MouseState {
    displays: Vec<mouse::Display>,
    map: mouse::MonitorMap,
    last_layout_check: Instant,
    health: Cams<Option<SignalFault>>,
    next_health: Instant,
}

impl MouseState {
    fn new(cfg: &Config) -> Result<Self> {
        let displays = mouse::displays()?;
        for d in &displays {
            info!("display {}", d.describe());
        }
        let map = mouse::MonitorMap::resolve(&cfg.cameras, &displays)?;
        for (cam, missing) in mouse::MonitorMap::missing(&cfg.cameras, &displays) {
            warn!("camera `{cam}`: configured display `{missing}` is not connected");
        }
        log_map(cfg, &map);
        Ok(Self {
            displays,
            map,
            last_layout_check: Instant::now(),
            health: Cams::filled(cfg.cameras.len(), None),
            next_health: Instant::now(),
        })
    }

    /// Re-reads the display layout (docking, unplugging) every few seconds.
    fn refresh_layout(&mut self, cfg: &Config) {
        if self.last_layout_check.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_layout_check = Instant::now();
        match mouse::displays() {
            Ok(d) if d != self.displays => {
                info!("display layout changed");
                self.displays = d;
                match mouse::MonitorMap::resolve(&cfg.cameras, &self.displays) {
                    Ok(map) => {
                        self.map = map;
                        log_map(cfg, &self.map);
                    }
                    Err(e) => warn!("{e:#}; keeping the previous mapping"),
                }
            }
            Ok(_) => {}
            Err(e) => warn!("reading display layout: {e:#}"),
        }
    }
}

fn log_map(cfg: &Config, map: &mouse::MonitorMap) {
    let parts: Vec<String> = cfg
        .cameras
        .iter()
        .zip(&map.by_cam)
        .map(|(c, devs)| {
            let devs: Vec<&str> = devs.iter().map(|d| d.trim_start_matches(r"\\.\")).collect();
            format!("{} <- [{}]", c.name, devs.join(", "))
        })
        .collect();
    info!(
        "mouse mapping: {}; other displays keep the current camera",
        parts.join(", ")
    );
}

pub async fn run(cfg: Config, opts: RunOptions) -> Result<()> {
    let mouse_mode = cfg.detector == DetectorKind::Mouse;
    let cal = match &cfg.calibration {
        Some(c) if c.fits(cfg.cameras.len()) => c.clone(),
        other => {
            if !mouse_mode {
                if other.is_some() {
                    warn!(
                        "calibration was made for a different number of cameras; ignoring it. Run `autocam calibrate`"
                    );
                } else {
                    warn!(
                        "not calibrated: assuming each camera points straight at you; run `autocam calibrate`"
                    );
                }
            }
            Calibration::default()
        }
    };
    let (pipeline, params, mouse_state) = if mouse_mode {
        info!("detector: mouse (the display under the cursor selects the camera)");
        (
            Pipeline::with_face(&cfg, false)?,
            cfg.mouse.switch_params(&cfg.switching),
            Some(MouseState::new(&cfg)?),
        )
    } else {
        info!("detector: face");
        (
            Pipeline::with_face(&cfg, opts.face)?,
            cfg.switching.params(),
            None,
        )
    };
    let status: SharedStatus = Arc::new(Mutex::new(Status {
        version: env!("CARGO_PKG_VERSION"),
        detector: cfg.detector,
        mode: cfg.switching.mode,
        calibrated: cfg
            .calibration
            .as_ref()
            .is_some_and(|c| c.fits(cfg.cameras.len())),
        cams: Cams(
            cfg.cameras
                .iter()
                .map(|c| diagnostics::CamStatus {
                    name: c.name.clone(),
                    ..Default::default()
                })
                .collect(),
        ),
        ..Default::default()
    }));
    info!(
        "cameras: {}",
        cfg.cameras
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{} {} (scene `{}`)", i + 1, c.name, c.scene))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let (ctl_tx, mut ctl_rx) = mpsc::channel::<Control>(64);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                info!("Ctrl+C: shutting down");
                tx.send(true).ok();
            }
        });
    }
    if opts.console {
        crate::control::spawn_console(ctl_tx.clone());
        println!("{CONSOLE_HELP}");
    }
    if cfg.diagnostics.hotkeys {
        crate::control::spawn_hotkeys(ctl_tx.clone(), cfg.cameras.len());
    }
    if cfg.diagnostics.http_enabled {
        let (bind, st, tx) = (cfg.diagnostics.http_bind, status.clone(), ctl_tx.clone());
        tokio::spawn(async move {
            if let Err(e) = diagnostics::serve_http(bind, st, tx).await {
                error!("diagnostics endpoint failed: {e:#}");
            }
        });
    }
    // Resource sampling and periodic summary.
    {
        let st = status.clone();
        let every = Duration::from_secs(cfg.diagnostics.summary_interval_s.max(1));
        tokio::spawn(async move {
            let mut sampler = ResourceSampler::new();
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            let mut last_log = Instant::now();
            loop {
                tick.tick().await;
                let r = sampler.sample();
                let line = {
                    let mut s = diagnostics::lock(&st);
                    s.resources = r;
                    s.one_line()
                };
                if last_log.elapsed() >= every {
                    info!("{line}");
                    last_log = Instant::now();
                }
            }
        });
    }

    let mut runner = Runner {
        switcher: Switcher::new(params, cfg.switching.mode),
        eval: EvalTracker::new(Duration::from_secs(5)),
        cfg,
        opts,
        cal,
        pipeline,
        status,
        last_auto: None,
        started: Instant::now(),
        mouse: mouse_state,
    };

    'outer: loop {
        let Some(obs) = connect_with_backoff(&runner.cfg.obs, &mut shutdown_rx).await else {
            break;
        };
        runner.on_connected(&obs).await;
        let mut events = match obs.events() {
            Ok(e) => Some(e),
            Err(e) => {
                warn!(
                    "no OBS event stream ({e:#}); external scene changes will be picked up by polling"
                );
                None
            }
        };
        let period = if runner.mouse.is_some() {
            runner.cfg.mouse.poll_period()
        } else {
            runner.cfg.capture.period()
        };
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut consecutive_failures = 0u32;
        let mut poll_program = tokio::time::interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if runner.tick(&obs).await {
                        consecutive_failures = 0;
                    } else {
                        consecutive_failures += 1;
                        if consecutive_failures >= 3 && obs.version().await.is_err() {
                            warn!("OBS stopped responding; reconnecting");
                            break;
                        }
                    }
                }
                ev = async { match events.as_mut() { Some(e) => e.next().await, None => std::future::pending().await } } => {
                    match ev {
                        Some(obws::events::Event::CurrentProgramSceneChanged { id }) => {
                            runner.on_program_scene(&id.name);
                        }
                        Some(obws::events::Event::ServerStopping | obws::events::Event::ExitStarted) => {
                            info!("OBS is shutting down");
                            break;
                        }
                        Some(_) => {}
                        None => {
                            warn!("OBS event stream ended; reconnecting");
                            break;
                        }
                    }
                }
                _ = poll_program.tick(), if events.is_none() => {
                    if let Ok(name) = obs.program_scene().await {
                        runner.on_program_scene(&name);
                    }
                }
                Some(c) = ctl_rx.recv() => {
                    if c == Control::Quit {
                        break 'outer;
                    }
                    runner.on_control(&obs, c).await;
                }
                _ = shutdown_rx.changed() => break 'outer,
            }
        }
        {
            let mut s = diagnostics::lock(&runner.status);
            s.obs_connected = false;
            s.reconnects += 1;
        }
        drop(obs);
    }
    info!("final: {}", diagnostics::lock(&runner.status).one_line());
    if runner.eval.stats.label_changes > 0 {
        info!(
            "evaluation: {}",
            serde_json::to_string(&runner.eval.stats).unwrap_or_default()
        );
    }
    Ok(())
}

impl Runner {
    async fn on_connected(&mut self, obs: &Obs) {
        let problems = match obs.check_setup(&self.cfg).await {
            Ok(p) => p,
            Err(e) => vec![format!("could not check OBS setup: {e:#}")],
        };
        for p in &problems {
            warn!("OBS setup: {p}");
        }
        let current = obs.program_scene().await.ok();
        let cam = current.as_deref().and_then(|n| self.cfg.cam_for_scene(n));
        self.switcher.handle(Event::ProgramScene {
            now: Instant::now(),
            cam,
        });
        let mut s = diagnostics::lock(&self.status);
        s.obs_connected = true;
        s.setup_problems = problems;
        s.current = self.switcher.current();
    }

    fn on_program_scene(&mut self, name: &str) {
        let cam = self.cfg.cam_for_scene(name);
        if cam != self.switcher.current() {
            info!("program scene changed in OBS to `{name}`; adopting");
        }
        self.switcher.handle(Event::ProgramScene {
            now: Instant::now(),
            cam,
        });
        diagnostics::lock(&self.status).current = self.switcher.current();
    }

    /// Updates per-camera status counters from one sample round.
    fn record_samples(&self, samples: &Cams<CamSample>) {
        let mut s = diagnostics::lock(&self.status);
        for (cam, smp) in samples.iter() {
            let Some(cs) = s.cams.0.get_mut(cam.0) else {
                continue;
            };
            cs.frames += 1;
            cs.signal = Some(smp.signal);
            cs.pose = smp.observation.map(|o| o.pose);
            match smp.signal {
                CamSignal::Face(_) => cs.faces += 1,
                CamSignal::Fault { .. } => cs.faults += 1,
                CamSignal::NoFace => {}
            }
            if let Some(ms) = smp.capture_ms {
                cs.capture_ms.push(ms);
            }
            if let Some(ms) = smp.inference_ms {
                cs.inference_ms.push(ms);
            }
        }
    }

    /// Mouse detector tick: cursor display -> camera, plus periodic camera
    /// health screenshots. Returns false if a health check reached no camera.
    async fn tick_mouse(&mut self, obs: &Obs) -> bool {
        let t0 = Instant::now();
        let Some(mut m) = self.mouse.take() else {
            return true;
        };
        let mut any_ok = true;
        let fps = self.cfg.mouse.health_check_fps;
        if fps > 0.0 && t0 >= m.next_health {
            m.next_health = t0 + Duration::from_secs_f32(1.0 / fps);
            let samples = self.pipeline.sample(obs, &self.cfg, &self.cal).await;
            any_ok = samples.0.iter().any(|s| s.error.is_none());
            for (cam, smp) in samples.iter() {
                let fault = match smp.signal {
                    CamSignal::Fault { fault } => Some(fault),
                    _ => None,
                };
                if fault != *m.health.get(cam) {
                    let name = self.cfg.cam_name(cam);
                    match fault {
                        Some(f) => {
                            warn!("camera `{name}` unavailable ({f:?}); will not switch to it")
                        }
                        None => info!("camera `{name}` healthy"),
                    }
                }
                *m.health.get_mut(cam) = fault;
            }
            self.record_samples(&samples);
        }
        m.refresh_layout(&self.cfg);

        let (target, display) = mouse::target(mouse::cursor_pos(), &m.displays, &m.map);
        let decision = mouse::decide(target, &m.health);
        self.mouse = Some(m);

        let now = Instant::now();
        let prev = self.switcher.current();
        let cmd = self.switcher.handle(Event::Tick { now, decision });
        self.eval.on_tick(now);
        {
            let mut s = diagnostics::lock(&self.status);
            s.uptime_s = self.started.elapsed().as_secs();
            s.ticks += 1;
            s.decision = Some(decision);
            s.pending = self.switcher.pending().map(|(c, _)| c);
            s.cursor_display.clone_from(&display);
        }
        if let Some(cmd) = cmd {
            let detail = display.map(|d| format!(" cursor on {}", d.trim_start_matches(r"\\.\")));
            self.apply(obs, cmd, prev, detail).await;
        }
        diagnostics::lock(&self.status)
            .tick_ms
            .push(t0.elapsed().as_secs_f32() * 1000.0);
        any_ok
    }

    /// Returns false if no camera could be captured.
    async fn tick(&mut self, obs: &Obs) -> bool {
        if self.mouse.is_some() {
            return self.tick_mouse(obs).await;
        }
        let t0 = Instant::now();
        let samples = self.pipeline.sample(obs, &self.cfg, &self.cal).await;
        let any_ok = samples.0.iter().any(|s| s.error.is_none());
        for (cam, smp) in samples.iter() {
            if let Some(e) = &smp.error {
                debug!("camera `{}`: capture failed: {e}", self.cfg.cam_name(cam));
            }
        }
        let signals = Cams(samples.0.iter().map(|s| s.signal).collect());
        let decision = scoring::decide(&signals, self.cfg.switching.decision());
        let now = Instant::now();
        let prev = self.switcher.current();
        let cmd = self.switcher.handle(Event::Tick { now, decision });
        self.eval.on_tick(now);
        debug!("tick {:?} -> {decision:?}", signals.0);

        self.record_samples(&samples);
        {
            let mut s = diagnostics::lock(&self.status);
            s.uptime_s = self.started.elapsed().as_secs();
            s.ticks += 1;
            s.decision = Some(decision);
            s.pending = self.switcher.pending().map(|(c, _)| c);
        }
        if let Some(cmd) = cmd {
            let scores: Vec<String> = signals
                .iter()
                .map(|(c, s)| {
                    format!(
                        "{}={}",
                        self.cfg.cam_name(c),
                        s.score().map_or("-".into(), |v| format!("{v:.2}"))
                    )
                })
                .collect();
            self.apply(
                obs,
                cmd,
                prev,
                Some(format!(" scores {}", scores.join(" "))),
            )
            .await;
        }
        diagnostics::lock(&self.status)
            .tick_ms
            .push(t0.elapsed().as_secs_f32() * 1000.0);
        any_ok
    }

    async fn apply(
        &mut self,
        obs: &Obs,
        cmd: SwitchCommand,
        prev: Option<CamId>,
        detail: Option<String>,
    ) {
        let scene = self.cfg.camera(cmd.to).scene.clone();
        let t = Instant::now();
        let result = obs.set_program_scene(&scene).await;
        let ack_ms = t.elapsed().as_secs_f32() * 1000.0;
        let now = Instant::now();
        match result {
            Ok(()) => {
                let decision_ms = t.saturating_duration_since(cmd.since).as_secs_f32() * 1000.0;
                info!(
                    "switch {} -> {} ({:?}) decision={decision_ms:.0}ms obs_ack={ack_ms:.0}ms{}",
                    prev.map_or("-", |p| self.cfg.cam_name(p)),
                    self.cfg.cam_name(cmd.to),
                    cmd.cause,
                    detail.unwrap_or_default()
                );
                self.eval.on_switch(now, cmd.to, cmd.cause);
                let mut s = diagnostics::lock(&self.status);
                s.current = Some(cmd.to);
                s.switches.obs_ack_ms.push(ack_ms);
                match cmd.cause {
                    SwitchCause::Manual => s.switches.manual += 1,
                    SwitchCause::Auto => {
                        s.switches.auto += 1;
                        s.switches.decision_latency_ms.push(decision_ms);
                        let window = Duration::from_millis(self.cfg.diagnostics.reversal_window_ms);
                        if let Some((at, from, to)) = self.last_auto
                            && Some(to) == prev
                            && from == cmd.to
                            && now.saturating_duration_since(at) < window
                        {
                            s.switches.reversals += 1;
                            warn!("likely false switch: reversed within {window:?}");
                        }
                        if let Some(p) = prev {
                            self.last_auto = Some((now, p, cmd.to));
                        }
                    }
                }
                s.eval = self.eval.stats.clone();
            }
            Err(e) => {
                error!("switching to scene `{scene}` failed: {e}");
                self.switcher.handle(Event::SwitchFailed {
                    now,
                    previous: prev,
                });
                diagnostics::lock(&self.status).switches.failures += 1;
            }
        }
    }

    /// Turns a user's camera reference into a camera, relative to the
    /// current one for next/previous.
    fn resolve(&self, r: &CamRef) -> Option<CamId> {
        let n = self.cfg.cameras.len();
        match r {
            CamRef::Id(c) => (c.0 < n).then_some(*c),
            CamRef::Key(k) => self.cfg.find_camera(k),
            CamRef::Next | CamRef::Prev => {
                let step = if *r == CamRef::Next { 1 } else { n - 1 };
                Some(match self.switcher.current() {
                    Some(c) => CamId((c.0 + step) % n),
                    None => CamId(0),
                })
            }
        }
    }

    async fn on_control(&mut self, obs: &Obs, c: Control) {
        let now = Instant::now();
        match c {
            Control::Select(r) => {
                let Some(cam) = self.resolve(&r) else {
                    warn!("no camera {r:?}; cameras are {}", self.camera_list());
                    return;
                };
                let prev = self.switcher.current();
                if let Some(cmd) = self.switcher.handle(Event::ManualSelect { now, cam }) {
                    info!(
                        "manual select `{}` (auto switching paused; Ctrl+Alt+A or `a` resumes)",
                        self.cfg.cam_name(cam)
                    );
                    self.apply(obs, cmd, prev, None).await;
                }
                self.mode_changed();
            }
            Control::SetMode(mode) => {
                self.switcher.handle(Event::SetMode { now, mode });
                info!("mode: {mode:?}");
                self.mode_changed();
            }
            Control::ToggleMode => {
                let mode = match self.switcher.mode() {
                    Mode::Auto => Mode::Manual,
                    Mode::Manual => Mode::Auto,
                };
                self.switcher.handle(Event::SetMode { now, mode });
                info!("mode: {mode:?}");
                self.mode_changed();
            }
            Control::Label(r) => {
                let label = match &r {
                    None => None,
                    Some(r) => match self.resolve(r) {
                        Some(c) => Some(c),
                        None => {
                            warn!("no camera {r:?}; cameras are {}", self.camera_list());
                            return;
                        }
                    },
                };
                self.eval.on_label(now, label, self.switcher.current());
                info!(
                    "ground-truth label: {}",
                    label.map_or("none (should not switch)", |c| self.cfg.cam_name(c))
                );
                let mut s = diagnostics::lock(&self.status);
                s.label = self.eval.label();
                s.eval = self.eval.stats.clone();
            }
            Control::PrintStatus => {
                println!("{}", diagnostics::lock(&self.status).one_line());
            }
            Control::Quit => {}
        }
    }

    fn camera_list(&self) -> String {
        self.cfg
            .cameras
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{} {}", i + 1, c.name))
            .collect::<Vec<_>>()
            .join(", ")
    }
    /// Updates status and persists the mode so it survives restarts.
    fn mode_changed(&mut self) {
        let mode = self.switcher.mode();
        diagnostics::lock(&self.status).mode = mode;
        if self.cfg.switching.mode != mode {
            self.cfg.switching.mode = mode;
            // Re-read so concurrent edits (e.g. calibration) are not clobbered.
            let saved = Config::load(&self.opts.config_path).and_then(|mut on_disk| {
                on_disk.switching.mode = mode;
                on_disk.save(&self.opts.config_path)
            });
            if let Err(e) = saved {
                warn!("could not save mode to config: {e:#}");
            }
        }
    }
}

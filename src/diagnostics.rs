//! Diagnostics: live status, latency statistics, ground-truth evaluation,
//! process resource sampling, and a loopback-only HTTP endpoint.
//!
//! Nothing here touches pixels. The status contains poses, scores and
//! counters only.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Serialize, Serializer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::DetectorKind;
use crate::control::{CamRef, Control};
use crate::face::HeadPose;
use crate::scoring::{CamSignal, Decision};
use crate::switching::{Mode, SwitchCause};
use crate::{CamId, Cams};

// ---------------------------------------------------------------------------
// Latency statistics
// ---------------------------------------------------------------------------

/// Bounded sample window that serializes as a summary.
#[derive(Debug, Clone)]
pub struct Samples {
    values: VecDeque<f32>,
    cap: usize,
    total: u64,
}

impl Default for Samples {
    fn default() -> Self {
        Self::with_capacity(2048)
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Summary {
    pub n: u64,
    pub mean: f32,
    pub p50: f32,
    pub p95: f32,
    pub max: f32,
}

impl Samples {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            values: VecDeque::with_capacity(cap.min(4096)),
            cap,
            total: 0,
        }
    }

    pub fn push(&mut self, v: f32) {
        if self.values.len() == self.cap {
            self.values.pop_front();
        }
        self.values.push_back(v);
        self.total += 1;
    }

    pub fn summary(&self) -> Option<Summary> {
        if self.values.is_empty() {
            return None;
        }
        let mut v: Vec<f32> = self.values.iter().copied().collect();
        v.sort_by(f32::total_cmp);
        let pct = |p: f32| v[((v.len() - 1) as f32 * p).round() as usize];
        Some(Summary {
            n: self.total,
            mean: v.iter().sum::<f32>() / v.len() as f32,
            p50: pct(0.5),
            p95: pct(0.95),
            max: *v.last().expect("non-empty"),
        })
    }
}

impl Serialize for Samples {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.summary().serialize(s)
    }
}

// ---------------------------------------------------------------------------
// Ground-truth evaluation (labels from hotkeys / HTTP / simulator)
// ---------------------------------------------------------------------------

/// Measures switch latency and false switches against ground-truth labels.
///
/// A label says where the user is actually looking: `Some(cam)` = that
/// camera's monitor, `None` = neither (keyboard, phone, away) where the
/// correct behaviour is to keep the current camera.
#[derive(Debug, Clone, Serialize, Default)]
pub struct EvalStats {
    /// Seconds during which a label was active.
    pub labelled_seconds: f64,
    pub label_changes: u64,
    /// Auto switches to the labelled camera.
    pub correct_switches: u64,
    /// Auto switches to a camera the user was not looking at.
    pub false_switches: u64,
    /// Label moved to the other camera but no switch followed within the timeout.
    pub missed_switches: u64,
    /// Time from label change to the matching switch being acknowledged by OBS.
    pub latency_ms: Samples,
    pub false_switches_per_hour: f64,
}

#[derive(Debug, Clone)]
pub struct EvalTracker {
    label: Option<Option<CamId>>,
    label_since: Option<Instant>,
    awaiting: Option<(CamId, Instant)>,
    miss_timeout: Duration,
    pub stats: EvalStats,
}

impl EvalTracker {
    pub fn new(miss_timeout: Duration) -> Self {
        Self {
            label: None,
            label_since: None,
            awaiting: None,
            miss_timeout,
            stats: EvalStats::default(),
        }
    }

    pub fn label(&self) -> Option<Option<CamId>> {
        self.label
    }

    fn accumulate(&mut self, now: Instant) {
        if let Some(since) = self.label_since {
            self.stats.labelled_seconds += now.saturating_duration_since(since).as_secs_f64();
            self.label_since = Some(now);
        }
        let hours = self.stats.labelled_seconds / 3600.0;
        if hours > 0.0 {
            self.stats.false_switches_per_hour = self.stats.false_switches as f64 / hours;
        }
    }

    pub fn on_label(&mut self, now: Instant, label: Option<CamId>, current: Option<CamId>) {
        self.accumulate(now);
        if self.label == Some(label) {
            return;
        }
        if self.label_since.is_none() {
            self.label_since = Some(now);
        }
        self.label = Some(label);
        self.stats.label_changes += 1;
        self.awaiting = match label {
            Some(c) if current != Some(c) => Some((c, now)),
            _ => None,
        };
    }

    pub fn on_switch(&mut self, now: Instant, to: CamId, cause: SwitchCause) {
        self.accumulate(now);
        if cause == SwitchCause::Manual {
            if self.awaiting.is_some_and(|(c, _)| c == to) {
                self.awaiting = None;
            }
            return;
        }
        match self.label {
            None => {}
            Some(Some(c)) if c == to => {
                self.stats.correct_switches += 1;
                if let Some((a, since)) = self.awaiting.take()
                    && a == to
                {
                    self.stats
                        .latency_ms
                        .push(now.saturating_duration_since(since).as_secs_f32() * 1000.0);
                }
            }
            Some(_) => self.stats.false_switches += 1,
        }
        self.accumulate(now);
    }

    pub fn on_tick(&mut self, now: Instant) {
        self.accumulate(now);
        if let Some((_, since)) = self.awaiting
            && now.saturating_duration_since(since) > self.miss_timeout
        {
            self.stats.missed_switches += 1;
            self.awaiting = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Live status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Default)]
pub struct CamStatus {
    /// Camera name from the config.
    pub name: String,
    pub signal: Option<CamSignal>,
    pub pose: Option<HeadPose>,
    pub frames: u64,
    pub faces: u64,
    pub faults: u64,
    pub capture_ms: Samples,
    pub inference_ms: Samples,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SwitchStats {
    pub auto: u64,
    pub manual: u64,
    /// Auto switches that undid the previous auto switch within the
    /// reversal window: a proxy for false switches without labels.
    pub reversals: u64,
    /// First opposing verdict -> switch command (dominated by the dwell).
    pub decision_latency_ms: Samples,
    /// SetCurrentProgramScene round-trip.
    pub obs_ack_ms: Samples,
    pub failures: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Resources {
    /// Percent of the whole machine (all logical cores).
    pub cpu_percent: f32,
    /// Percent of one core.
    pub cpu_percent_one_core: f32,
    pub rss_mb: f32,
    pub obs_cpu_percent: Option<f32>,
    pub obs_rss_mb: Option<f32>,
    pub logical_cores: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub version: &'static str,
    pub detector: DetectorKind,
    pub uptime_s: u64,
    pub obs_connected: bool,
    pub reconnects: u64,
    pub mode: Mode,
    /// Mouse detector: display under the cursor.
    pub cursor_display: Option<String>,
    pub current: Option<CamId>,
    pub pending: Option<CamId>,
    pub calibrated: bool,
    pub decision: Option<Decision>,
    /// One entry per camera, in camera order (`current`/`pending` index it).
    pub cams: Cams<CamStatus>,
    pub ticks: u64,
    pub tick_ms: Samples,
    pub switches: SwitchStats,
    pub eval: EvalStats,
    pub label: Option<Option<CamId>>,
    pub resources: Resources,
    pub setup_problems: Vec<String>,
}

pub type SharedStatus = Arc<Mutex<Status>>;

pub fn lock(s: &SharedStatus) -> std::sync::MutexGuard<'_, Status> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

impl Status {
    /// Camera name for a `current`/`pending` index.
    pub fn cam_name(&self, cam: CamId) -> &str {
        self.cams.0.get(cam.0).map_or("?", |s| s.name.as_str())
    }

    pub fn one_line(&self) -> String {
        let mouse = self.detector == DetectorKind::Mouse;
        let cam = |s: &CamStatus| {
            let name = &s.name;
            if mouse {
                let health = match s.signal {
                    Some(CamSignal::Fault { fault }) => format!("{fault:?}").to_lowercase(),
                    Some(_) => "ok".into(),
                    None => "-".into(),
                };
                return format!("{name}={health}");
            }
            let face_rate = if s.frames == 0 {
                0.0
            } else {
                100.0 * s.faces as f32 / s.frames as f32
            };
            let score = match s.signal {
                Some(CamSignal::Face(sc)) => format!("{:.2}", sc.score),
                Some(CamSignal::NoFace) => "noface".into(),
                Some(CamSignal::Fault { fault }) => format!("{fault:?}").to_lowercase(),
                None => "-".into(),
            };
            format!("{name}={score} face={face_rate:.0}%")
        };
        let cams: Vec<String> = self.cams.0.iter().map(cam).collect();
        let cursor = if mouse {
            format!(
                " cursor={}",
                self.cursor_display
                    .as_deref()
                    .map_or("-", |d| d.trim_start_matches(r"\\.\"))
            )
        } else {
            String::new()
        };
        format!(
            "mode={:?} cur={}{cursor} {} switches={}a/{}m rev={} cpu={:.1}% ({:.0}% core) rss={:.0}MB obs_cpu={} tick_p95={}",
            self.mode,
            self.current.map_or("-", |c| self.cam_name(c)),
            cams.join(" "),
            self.switches.auto,
            self.switches.manual,
            self.switches.reversals,
            self.resources.cpu_percent,
            self.resources.cpu_percent_one_core,
            self.resources.rss_mb,
            self.resources
                .obs_cpu_percent
                .map_or("-".into(), |c| format!("{c:.1}%")),
            self.tick_ms
                .summary()
                .map_or("-".into(), |s| format!("{:.0}ms", s.p95)),
        )
    }
}

// ---------------------------------------------------------------------------
// Resource sampling
// ---------------------------------------------------------------------------

pub struct ResourceSampler {
    sys: sysinfo::System,
    me: sysinfo::Pid,
    cores: usize,
}

impl Default for ResourceSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceSampler {
    pub fn new() -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
        let cores = sys.cpus().len().max(1);
        Self {
            sys,
            me: sysinfo::Pid::from_u32(std::process::id()),
            cores,
        }
    }

    /// CPU figures are averages since the previous call.
    pub fn sample(&mut self) -> Resources {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let mut r = Resources {
            logical_cores: self.cores,
            ..Default::default()
        };
        if let Some(p) = self.sys.process(self.me) {
            r.cpu_percent_one_core = p.cpu_usage();
            r.cpu_percent = p.cpu_usage() / self.cores as f32;
            r.rss_mb = p.memory() as f32 / (1024.0 * 1024.0);
        }
        let mut obs_cpu = 0.0;
        let mut obs_mem = 0u64;
        let mut found = false;
        for p in self.sys.processes_by_exact_name("obs64.exe".as_ref()) {
            found = true;
            obs_cpu += p.cpu_usage();
            obs_mem += p.memory();
        }
        if found {
            r.obs_cpu_percent = Some(obs_cpu / self.cores as f32);
            r.obs_rss_mb = Some(obs_mem as f32 / (1024.0 * 1024.0));
        }
        r
    }
}

// ---------------------------------------------------------------------------
// Loopback HTTP endpoint
// ---------------------------------------------------------------------------

/// Serves:
/// - `GET /status` (JSON) and `GET /` (one-line text)
/// - `POST /select/{left,right}`, `/mode/{auto,manual,toggle}`,
///   `/label/{left,right,none}`
///
/// Hardening: binds loopback only; rejects requests whose `Host` is not a
/// loopback name (DNS-rebinding); POSTs need an `X-AutoCam: 1` header, which
/// browsers cannot send cross-origin without a CORS preflight we never grant.
pub async fn serve_http(
    bind: SocketAddr,
    status: SharedStatus,
    control: mpsc::Sender<Control>,
) -> Result<()> {
    if !bind.ip().is_loopback() {
        bail!("refusing to serve diagnostics on non-loopback address {bind}");
    }
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding diagnostics endpoint {bind}"))?;
    info!("diagnostics on http://{bind}/status");
    loop {
        let (sock, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let status = status.clone();
        let control = control.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(sock, bind, status, control).await {
                debug!("diagnostics connection: {e:#}");
            }
        });
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Status,
    Text,
    Control(Control),
}

#[derive(Debug, PartialEq, Eq)]
pub enum HttpReject {
    BadRequest,
    Forbidden,
    NotFound,
    MethodNotAllowed,
}

/// Pure request routing, unit-tested without sockets.
pub fn route(request_head: &str, port: u16) -> Result<Route, HttpReject> {
    let mut lines = request_head.split("\r\n");
    let mut first = lines.next().ok_or(HttpReject::BadRequest)?.split(' ');
    let (method, path) = (
        first.next().ok_or(HttpReject::BadRequest)?,
        first.next().ok_or(HttpReject::BadRequest)?,
    );
    let mut host = None;
    let mut token = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let v = v.trim();
            if k.eq_ignore_ascii_case("host") {
                host = Some(v.to_owned());
            } else if k.eq_ignore_ascii_case("x-autocam") && v == "1" {
                token = true;
            }
        }
    }
    let allowed_hosts = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    if !host.is_some_and(|h| allowed_hosts.iter().any(|a| a.eq_ignore_ascii_case(&h))) {
        return Err(HttpReject::Forbidden);
    }
    match method {
        "GET" => match path {
            "/status" => Ok(Route::Status),
            "/" => Ok(Route::Text),
            _ => Err(HttpReject::NotFound),
        },
        "POST" => {
            if !token {
                return Err(HttpReject::Forbidden);
            }
            let c = match path {
                "/mode/auto" => Control::SetMode(Mode::Auto),
                "/mode/manual" => Control::SetMode(Mode::Manual),
                "/mode/toggle" => Control::ToggleMode,
                "/label/none" => Control::Label(None),
                p => {
                    if let Some(key) = p.strip_prefix("/select/") {
                        Control::Select(CamRef::parse(key).ok_or(HttpReject::NotFound)?)
                    } else if let Some(key) = p.strip_prefix("/label/") {
                        Control::Label(Some(CamRef::parse(key).ok_or(HttpReject::NotFound)?))
                    } else {
                        return Err(HttpReject::NotFound);
                    }
                }
            };
            Ok(Route::Control(c))
        }
        _ => Err(HttpReject::MethodNotAllowed),
    }
}

async fn handle_conn(
    mut sock: TcpStream,
    bind: SocketAddr,
    status: SharedStatus,
    control: mpsc::Sender<Control>,
) -> Result<()> {
    let mut buf = vec![0u8; 8192];
    let mut n = 0;
    let head = loop {
        let read = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf[n..]))
            .await
            .context("read timeout")??;
        if read == 0 {
            return Ok(());
        }
        n += read;
        if let Some(end) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
            break String::from_utf8_lossy(&buf[..end]).into_owned();
        }
        if n == buf.len() {
            bail!("request head too large");
        }
    };
    let (code, ctype, body) = match route(&head, bind.port()) {
        Ok(Route::Status) => {
            let s = lock(&status).clone();
            (
                "200 OK",
                "application/json",
                serde_json::to_string_pretty(&s)?,
            )
        }
        Ok(Route::Text) => ("200 OK", "text/plain", lock(&status).one_line() + "\n"),
        Ok(Route::Control(c)) => {
            if control.send(c).await.is_err() {
                warn!("control channel closed");
            }
            ("204 No Content", "text/plain", String::new())
        }
        Err(HttpReject::BadRequest) => ("400 Bad Request", "text/plain", "bad request\n".into()),
        Err(HttpReject::Forbidden) => ("403 Forbidden", "text/plain", "forbidden\n".into()),
        Err(HttpReject::NotFound) => ("404 Not Found", "text/plain", "not found\n".into()),
        Err(HttpReject::MethodNotAllowed) => (
            "405 Method Not Allowed",
            "text/plain",
            "method not allowed\n".into(),
        ),
    };
    let resp = format!(
        "HTTP/1.1 {code}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(resp.as_bytes()).await?;
    sock.shutdown().await.ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, path: &str, host: &str, token: bool) -> String {
        let mut s = format!("{method} {path} HTTP/1.1\r\nHost: {host}");
        if token {
            s.push_str("\r\nX-AutoCam: 1");
        }
        s
    }

    #[test]
    fn routes_and_hardening() {
        assert_eq!(
            route(&req("GET", "/status", "127.0.0.1:8787", false), 8787),
            Ok(Route::Status)
        );
        assert_eq!(
            route(&req("POST", "/select/left", "localhost:8787", true), 8787),
            Ok(Route::Control(Control::Select(CamRef::Key("left".into()))))
        );
        // DNS rebinding: attacker-controlled host name.
        assert_eq!(
            route(&req("GET", "/status", "evil.example:8787", false), 8787),
            Err(HttpReject::Forbidden)
        );
        // CSRF: POST without the custom header.
        assert_eq!(
            route(&req("POST", "/select/left", "127.0.0.1:8787", false), 8787),
            Err(HttpReject::Forbidden)
        );
        assert_eq!(
            route(&req("DELETE", "/status", "127.0.0.1:8787", true), 8787),
            Err(HttpReject::MethodNotAllowed)
        );
        assert_eq!(
            route(&req("GET", "/nope", "127.0.0.1:8787", false), 8787),
            Err(HttpReject::NotFound)
        );
        assert_eq!(route("", 8787), Err(HttpReject::BadRequest));
    }

    #[test]
    fn camera_routes() {
        let post = |p: &str| route(&req("POST", p, "127.0.0.1:8787", true), 8787);
        assert_eq!(
            post("/select/3"),
            Ok(Route::Control(Control::Select(CamRef::Key("3".into()))))
        );
        assert_eq!(
            post("/select/next"),
            Ok(Route::Control(Control::Select(CamRef::Next)))
        );
        assert_eq!(
            post("/label/laptop"),
            Ok(Route::Control(Control::Label(Some(CamRef::Key(
                "laptop".into()
            )))))
        );
        assert_eq!(
            post("/label/none"),
            Ok(Route::Control(Control::Label(None)))
        );
        assert_eq!(post("/select/"), Err(HttpReject::NotFound));
        assert_eq!(post("/select/a%20b"), Err(HttpReject::NotFound));
    }

    #[test]
    fn samples_summary() {
        let mut s = Samples::with_capacity(3);
        assert!(s.summary().is_none());
        for v in [10.0, 20.0, 30.0, 40.0] {
            s.push(v);
        }
        let sum = s.summary().unwrap();
        assert_eq!(sum.n, 4);
        assert_eq!(sum.max, 40.0);
        assert_eq!(sum.p50, 30.0); // window keeps the last 3
    }

    #[test]
    fn eval_counts_latency_false_and_missed() {
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);
        let mut e = EvalTracker::new(Duration::from_secs(5));

        e.on_label(ms(0), Some(CamId(0)), Some(CamId(0)));
        e.on_label(ms(1000), Some(CamId(1)), Some(CamId(0)));
        e.on_switch(ms(2100), CamId(1), SwitchCause::Auto);
        assert_eq!(e.stats.correct_switches, 1);
        let lat = e.stats.latency_ms.summary().unwrap();
        assert!((lat.p50 - 1100.0).abs() < 1.0);

        // Looking at the keyboard: any auto switch is false.
        e.on_label(ms(3000), None, Some(CamId(1)));
        e.on_switch(ms(4000), CamId(0), SwitchCause::Auto);
        assert_eq!(e.stats.false_switches, 1);

        // Manual switches are never counted as false.
        e.on_switch(ms(4100), CamId(1), SwitchCause::Manual);
        assert_eq!(e.stats.false_switches, 1);

        // Label moves, no switch within the timeout: missed.
        e.on_label(ms(5000), Some(CamId(0)), Some(CamId(1)));
        e.on_tick(ms(9000));
        assert_eq!(e.stats.missed_switches, 0);
        e.on_tick(ms(10_500));
        assert_eq!(e.stats.missed_switches, 1);
        assert!((e.stats.labelled_seconds - 10.5).abs() < 0.01);
        assert!(e.stats.false_switches_per_hour > 0.0);
    }
}

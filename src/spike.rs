//! Feasibility spike: can OBS GetSourceScreenshot sample both cameras at
//! 4 FPS, including the one whose scene is *not* on Program, with fresh
//! frames?
//!
//! Method: sample both cameras' sources at `capture.fps` while rotating
//! Program between the LeftCam and RightCam scenes (and optionally an empty
//! idle scene). Each sample is classified per camera:
//!
//! - `selected`: this camera's own scene is on Program
//! - `other_selected`: the other camera's scene is on Program (the case that
//!   matters: the camera we must keep watching while the user is not on it)
//! - `neither`: some unrelated scene is on Program (informational only)
//!
//! Freshness = frame differs from the previous sample of that camera; a live
//! sensor is never byte-identical twice. The verdict needs >= 99% successful
//! screenshots, >= 95% fresh frames and p95 latency <= 150 ms in both
//! `selected` and `other_selected`.
//!
//! `--negative-control` repeats the run with "Deactivate when not showing"
//! on, which must make `other_selected` frames stale. That shows the test can
//! detect the failure it is looking for.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::Serialize;
use tracing::{info, warn};

use crate::capture::Frame;
use crate::config::Config;
use crate::diagnostics::{Samples, Summary};
use crate::obs::Obs;
use crate::{CamId, PerCam};

pub const IDLE_SCENE: &str = "AutoCam Spike Idle";

#[derive(Debug, Clone)]
pub struct SpikeOptions {
    pub seconds: u64,
    pub rotate_every: Duration,
    pub include_idle: bool,
    pub negative_control: bool,
    /// Disable the keepalive items for the main phase (A/B test), then restore.
    pub without_keepalive: bool,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct StateStats {
    pub samples: u64,
    pub failures: u64,
    pub fresh: u64,
    pub identical: u64,
    pub longest_identical_run: u32,
    /// Samples for which OBS reported the source active on Program.
    pub obs_active: u64,
    pub mean_abs_diff: f32,
    /// Mean frame brightness 0-255 (flat/dark frames compress identically).
    pub mean_luma: f32,
    pub latency_ms: Option<Summary>,
    #[serde(skip)]
    luma_sum: f64,
    #[serde(skip)]
    diff_sum: f64,
    #[serde(skip)]
    run: u32,
    #[serde(skip)]
    latency: Samples,
}

impl StateStats {
    fn finish(&mut self) {
        let compared = (self.fresh + self.identical).max(1);
        self.mean_abs_diff = (self.diff_sum / compared as f64) as f32;
        let ok = (self.samples - self.failures).max(1);
        self.mean_luma = (self.luma_sum / ok as f64) as f32;
        self.latency_ms = self.latency.summary();
    }

    pub fn success_rate(&self) -> f32 {
        if self.samples == 0 {
            0.0
        } else {
            1.0 - self.failures as f32 / self.samples as f32
        }
    }

    pub fn fresh_rate(&self) -> f32 {
        let n = self.fresh + self.identical;
        if n == 0 {
            0.0
        } else {
            self.fresh as f32 / n as f32
        }
    }
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct CamStats {
    pub source: String,
    pub selected: StateStats,
    pub other_selected: StateStats,
    pub neither: StateStats,
}

#[derive(Debug, Serialize, Clone)]
pub struct PhaseReport {
    pub label: String,
    pub keepalive: bool,
    pub cams: PerCam<CamStats>,
    pub scene_switch_ack_ms: Option<Summary>,
    pub achieved_fps_per_camera: f32,
    pub reliable: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct SpikeReport {
    pub obs_version: String,
    pub fps_target: f32,
    pub resolution: (u32, u32),
    pub main: PhaseReport,
    pub negative_control: Option<PhaseReport>,
    pub verdict: String,
}

pub async fn run_spike(cfg: &Config, opts: &SpikeOptions) -> Result<SpikeReport> {
    let obs = Obs::connect(&cfg.obs).await?;
    let version = obs.version().await?;
    info!("spike against {version}");
    // Missing keepalive is what the spike measures, so it is not fatal here.
    let problems: Vec<String> = obs
        .check_setup(cfg)
        .await?
        .into_iter()
        .filter(|p| !p.contains("keepalive"))
        .collect();
    if !problems.is_empty() {
        bail!("OBS setup incomplete: {}", problems.join("; "));
    }
    if opts.without_keepalive {
        let n = obs.set_keepalive_enabled(cfg, false).await?;
        info!("disabled {n} keepalive item(s) for this run; they are restored afterwards");
        // Give OBS a moment to stop rendering them.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let keepalive = obs.keepalive_ok(cfg).await?;
    let original_program = obs.program_scene().await?;

    let mut scenes = vec![
        cfg.cameras.left.scene.clone(),
        cfg.cameras.right.scene.clone(),
    ];
    if opts.include_idle {
        if !obs.scene_names().await?.contains(&IDLE_SCENE.to_owned()) {
            obs.client().scenes().create(IDLE_SCENE).await?;
            info!("created empty scene `{IDLE_SCENE}`");
        }
        scenes.push(IDLE_SCENE.to_owned());
    }

    let main = phase(&obs, cfg, opts, &scenes, keepalive, "main").await;
    if opts.without_keepalive {
        let n = obs.set_keepalive_enabled(cfg, true).await?;
        info!("re-enabled {n} keepalive item(s)");
    }

    let negative = if opts.negative_control {
        let inputs = obs.input_names().await?;
        let mut cams: Vec<String> = CamId::ALL
            .iter()
            .map(|c| cfg.camera(*c).source.clone())
            .filter(|s| inputs.contains(s))
            .collect();
        cams.dedup();
        for s in &cams {
            obs.set_deactivate_when_hidden(s, true).await?;
        }
        let short = SpikeOptions {
            seconds: (opts.seconds / 2).max(10),
            ..opts.clone()
        };
        let r = phase(&obs, cfg, &short, &scenes, keepalive, "negative-control").await;
        for s in &cams {
            obs.set_deactivate_when_hidden(s, false).await?;
            // Settings changes can leave a camera closed; make sure it is open.
            obs.reactivate_camera(s).await?;
        }
        Some(r?)
    } else {
        None
    };
    obs.set_program_scene(&original_program).await.ok();

    let main = main?;
    let verdict = if main.reliable {
        format!(
            "RELIABLE (keepalive {}): use OBS GetSourceScreenshot + SetCurrentProgramScene",
            if keepalive { "on" } else { "off" }
        )
    } else {
        format!(
            "UNRELIABLE (keepalive {}): {}",
            if keepalive { "on" } else { "off" },
            if keepalive {
                "use the native capture fallback (docs/DESIGN.md)"
            } else {
                "run `autocam obs-setup` to add the keepalive items and re-test"
            }
        )
    };
    if let Some(n) = &negative {
        if n.reliable {
            warn!("negative control did NOT detect stale frames; the freshness test may be blind");
        } else {
            info!("negative control detected stale frames as expected");
        }
    }
    Ok(SpikeReport {
        obs_version: version,
        fps_target: cfg.capture.fps,
        resolution: (cfg.capture.width, cfg.capture.height),
        main,
        negative_control: negative,
        verdict,
    })
}

async fn phase(
    obs: &Obs,
    cfg: &Config,
    opts: &SpikeOptions,
    scenes: &[String],
    keepalive: bool,
    label: &str,
) -> Result<PhaseReport> {
    let mut stats = PerCam::new(
        CamStats {
            source: cfg.cameras.left.source.clone(),
            ..Default::default()
        },
        CamStats {
            source: cfg.cameras.right.source.clone(),
            ..Default::default()
        },
    );
    let mut last: PerCam<Option<Frame>> = PerCam::default();
    let mut ack = Samples::default();
    let mut ticker = tokio::time::interval(cfg.capture.period());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start = Instant::now();
    let end = start + Duration::from_secs(opts.seconds);
    let mut next_rotate = start;
    let mut scene_idx = 0usize;
    let mut program = String::new();
    let mut ticks = 0u64;

    info!(
        "phase `{label}` (keepalive {}): {} s at {} FPS, rotating Program every {:?} through {:?}",
        if keepalive { "on" } else { "off" },
        opts.seconds,
        cfg.capture.fps,
        opts.rotate_every,
        scenes
    );
    while Instant::now() < end {
        ticker.tick().await;
        if Instant::now() >= next_rotate {
            program = scenes[scene_idx % scenes.len()].clone();
            let t = Instant::now();
            obs.set_program_scene(&program).await?;
            ack.push(t.elapsed().as_secs_f32() * 1000.0);
            scene_idx += 1;
            next_rotate += opts.rotate_every;
        }
        ticks += 1;
        for cam in CamId::ALL {
            let src = &cfg.camera(cam).source;
            let active = obs
                .source_active(src)
                .await
                .map(|(a, _)| a)
                .unwrap_or(false);
            let t = Instant::now();
            let shot = obs.screenshot(cam, src, &cfg.capture).await;
            let ms = t.elapsed().as_secs_f32() * 1000.0;
            let cs = stats.get_mut(cam);
            let st = if program == cfg.camera(cam).scene {
                &mut cs.selected
            } else if program == cfg.camera(cam.other()).scene {
                &mut cs.other_selected
            } else {
                &mut cs.neither
            };
            st.samples += 1;
            st.obs_active += u64::from(active);
            match shot {
                Ok(frame) => {
                    st.latency.push(ms);
                    st.luma_sum += f64::from(frame.mean_luma);
                    if let Some(prev) = last.get(cam) {
                        st.diff_sum += f64::from(prev.mean_abs_diff(&frame).unwrap_or(0.0));
                        if prev.fingerprint == frame.fingerprint {
                            st.identical += 1;
                            st.run += 1;
                            st.longest_identical_run = st.longest_identical_run.max(st.run);
                        } else {
                            st.fresh += 1;
                            st.run = 0;
                        }
                    }
                    // Only the latest frame per camera is kept, in memory.
                    *last.get_mut(cam) = Some(frame);
                }
                Err(e) => {
                    st.failures += 1;
                    warn!("{cam}: screenshot failed: {e}");
                }
            }
        }
    }
    drop(last);
    let elapsed = start.elapsed().as_secs_f32();

    let mut reasons = Vec::new();
    for cam in CamId::ALL {
        let cs = stats.get_mut(cam);
        cs.neither.finish();
        for (state, st) in [
            ("own scene on Program", &mut cs.selected),
            ("other camera's scene on Program", &mut cs.other_selected),
        ] {
            st.finish();
            if st.samples == 0 {
                reasons.push(format!("{cam} ({state}): never observed"));
                continue;
            }
            if st.success_rate() < 0.99 {
                reasons.push(format!(
                    "{cam} ({state}): {:.1}% of screenshots failed",
                    100.0 * (1.0 - st.success_rate())
                ));
            }
            if st.fresh_rate() < 0.95 {
                reasons.push(format!(
                    "{cam} ({state}): only {:.1}% fresh frames (longest identical run {})",
                    100.0 * st.fresh_rate(),
                    st.longest_identical_run
                ));
            }
            if let Some(l) = st.latency_ms
                && l.p95 > 150.0
            {
                reasons.push(format!(
                    "{cam} ({state}): p95 screenshot latency {:.0} ms",
                    l.p95
                ));
            }
        }
    }
    Ok(PhaseReport {
        label: label.to_owned(),
        keepalive,
        achieved_fps_per_camera: ticks as f32 / elapsed.max(0.001),
        scene_switch_ack_ms: ack.summary(),
        reliable: reasons.is_empty(),
        reasons,
        cams: stats,
    })
}

pub fn print_report(r: &SpikeReport) {
    println!("\n=== AutoCam feasibility spike ===");
    println!(
        "{}  target {} FPS/camera at {}x{}",
        r.obs_version, r.fps_target, r.resolution.0, r.resolution.1
    );
    for p in std::iter::once(&r.main).chain(r.negative_control.as_ref()) {
        println!(
            "\n-- phase: {} | keepalive {} | achieved {:.2} FPS/camera --",
            p.label,
            if p.keepalive { "on" } else { "off" },
            p.achieved_fps_per_camera
        );
        if let Some(a) = p.scene_switch_ack_ms {
            println!(
                "SetCurrentProgramScene ack: p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms (n={})",
                a.p50, a.p95, a.max, a.n
            );
        }
        println!(
            "{:<6} {:<20} {:<16} {:>7} {:>5} {:>7} {:>7} {:>6} {:>8} {:>6} {:>7} {:>7}",
            "camera",
            "source",
            "program shows",
            "samples",
            "fail",
            "fresh%",
            "longrun",
            "active",
            "meandiff",
            "luma",
            "p50 ms",
            "p95 ms"
        );
        for cam in CamId::ALL {
            let cs = p.cams.get(cam);
            for (state, st) in [
                ("own scene", &cs.selected),
                ("other cam scene", &cs.other_selected),
                ("neither", &cs.neither),
            ] {
                if st.samples == 0 {
                    continue;
                }
                let (p50, p95) = st
                    .latency_ms
                    .map_or((f32::NAN, f32::NAN), |l| (l.p50, l.p95));
                println!(
                    "{:<6} {:<20} {:<16} {:>7} {:>5} {:>6.1}% {:>7} {:>5.0}% {:>8.2} {:>6.1} {:>7.1} {:>7.1}",
                    cam.name(),
                    cs.source,
                    state,
                    st.samples,
                    st.failures,
                    100.0 * st.fresh_rate(),
                    st.longest_identical_run,
                    100.0 * st.obs_active as f32 / st.samples as f32,
                    st.mean_abs_diff,
                    st.mean_luma,
                    p50,
                    p95
                );
            }
        }
        println!("reliable: {}", p.reliable);
        for reason in &p.reasons {
            println!("  - {reason}");
        }
    }
    println!("\nVERDICT: {}\n", r.verdict);
}

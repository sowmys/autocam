//! `autocam calibrate` (interactive) and `autocam watch` (live readout), face mode.
//!
//! Calibration procedure, one step per camera plus an optional last one:
//! 1. Face the centre of camera 1's monitor, press Enter, hold ~3 s.
//! 2. Same for camera 2's monitor, and so on for every camera.
//! 3. Optionally look down at the keyboard, press Enter, hold ~3 s.
//!
//! Turn your head, not just your eyes: head orientation is what is measured.
//! Every camera is sampled in every step, so each camera learns the pose it
//! sees when you face its monitor and the poses it sees when you face the
//! others. That is what absorbs camera mounting angles and normalizes all
//! cameras' scores onto the same scale.

use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::Cams;
use crate::app::Pipeline;
use crate::config::Config;
use crate::obs::Obs;
use crate::scoring::{Calibration, CalibrationSamples, CamSignal, PhaseSamples, fit_calibration};

async fn wait_enter(prompt: &str) -> Result<()> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    tokio::task::spawn_blocking(|| {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).map(|_| ())
    })
    .await??;
    Ok(())
}

async fn collect(
    obs: &Obs,
    cfg: &Config,
    pipeline: &mut Pipeline,
    seconds: f32,
) -> Result<PhaseSamples> {
    let cal = Calibration::default();
    let mut out = PhaseSamples::new(cfg.cameras.len());
    let mut ticker = tokio::time::interval(cfg.capture.period());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let end = Instant::now() + Duration::from_secs_f32(seconds);
    // Skip the first half-second: the user is still settling.
    tokio::time::sleep(Duration::from_millis(500)).await;
    while Instant::now() < end {
        ticker.tick().await;
        let s = pipeline.sample(obs, cfg, &cal).await;
        out.frames += 1;
        for (cam, smp) in s.iter() {
            if let Some(o) = smp.observation {
                out.poses.get_mut(cam).push(o.pose);
            }
            if let CamSignal::Fault { fault } = smp.signal {
                println!("  camera `{}`: {fault:?}", cfg.cam_name(cam));
            }
        }
    }
    let seen: Vec<String> = out
        .poses
        .iter()
        .map(|(cam, p)| format!("{} {}", cfg.cam_name(cam), p.len()))
        .collect();
    println!(
        "  captured {} frames; face seen by {}",
        out.frames,
        seen.join(" / ")
    );
    Ok(out)
}

pub async fn calibrate(
    cfg: &Config,
    config_path: &Path,
    seconds: f32,
    look_down: bool,
) -> Result<()> {
    let obs = Obs::connect(&cfg.obs).await?;
    let problems = obs.check_setup(cfg).await?;
    anyhow::ensure!(
        problems.is_empty(),
        "OBS setup incomplete: {}",
        problems.join("; ")
    );
    let mut pipeline = Pipeline::new(cfg)?;
    let steps = cfg.cameras.len() + usize::from(look_down);

    println!("\nAutoCam calibration. Turn your HEAD (not just your eyes) for each step.\n");
    let mut phases = Vec::with_capacity(cfg.cameras.len());
    for (i, c) in cfg.cameras.iter().enumerate() {
        wait_enter(&format!(
            "Step {}/{steps}: face the centre of the monitor with camera `{}`, then press Enter and hold still... ",
            i + 1,
            c.name
        ))
        .await?;
        phases.push(collect(&obs, cfg, &mut pipeline, seconds).await?);
    }

    let look_down = if look_down {
        wait_enter(&format!(
            "Step {steps}/{steps}: look down at your keyboard, then press Enter and hold still... "
        ))
        .await?;
        Some(collect(&obs, cfg, &mut pipeline, seconds).await?)
    } else {
        None
    };

    let samples = CalibrationSamples { phases, look_down };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (cal, report) = fit_calibration(&samples, cfg.switching.confidence_margin, now)
        .context("calibration failed")?;

    println!("\nCalibration result:");
    for cam in cfg.cam_ids() {
        let c = cal.camera(cam);
        println!(
            "  {:>8}: target yaw {:+6.1} pitch {:+6.1} | nearest other {} | sigma yaw {:.1} pitch {:.1} | down below {:+.1} | noise {:.1} deg",
            cfg.cam_name(cam),
            c.target.yaw,
            c.target.pitch,
            c.away.map_or("face lost".to_owned(), |a| format!(
                "yaw {:+6.1} pitch {:+6.1}",
                a.yaw, a.pitch
            )),
            c.sigma_yaw,
            c.sigma_pitch,
            c.down_pitch,
            report.noise[cam.0],
        );
    }
    println!("  predicted scores (rows: facing that camera's monitor):");
    for (i, row) in report.predicted.iter().enumerate() {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(c, v)| format!("{}={v:.2}", cfg.cameras[c].name))
            .collect();
        println!("    {:>8}: {}", cfg.cameras[i].name, cells.join("  "));
    }
    for w in &report.warnings {
        println!("  WARNING: {w}");
    }

    // Merge into the file on disk so unrelated edits are preserved.
    let mut on_disk = Config::load(config_path)?;
    on_disk.calibration = Some(cal);
    on_disk.save(config_path)?;
    println!("\nSaved to {}", config_path.display());
    Ok(())
}

/// Prints live per-camera pose and score, to tune placement and lighting.
pub async fn watch(cfg: &Config, seconds: Option<u64>) -> Result<()> {
    let obs = Obs::connect(&cfg.obs).await?;
    let mut pipeline = Pipeline::new(cfg)?;
    let cal = cfg
        .calibration
        .clone()
        .filter(|c| c.fits(cfg.cameras.len()))
        .unwrap_or_default();
    let mut ticker = tokio::time::interval(cfg.capture.period());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let end = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    let header: Vec<String> = cfg
        .cameras
        .iter()
        .map(|c| format!("{}: yaw pitch score infer", c.name))
        .collect();
    println!("time    | {} | decision", header.join(" | "));
    let t0 = Instant::now();
    while end.is_none_or(|e| Instant::now() < e) {
        ticker.tick().await;
        let s = pipeline.sample(&obs, cfg, &cal).await;
        let cells: Vec<String> =
            s.0.iter()
                .map(|smp| {
                    let infer = smp
                        .inference_ms
                        .map_or("    -".into(), |m| format!("{m:4.0}ms"));
                    match (smp.observation, smp.signal) {
                        (Some(o), CamSignal::Face(sc)) => format!(
                            "{:+6.1} {:+6.1} {:5.2} {infer}",
                            o.pose.yaw, o.pose.pitch, sc.score
                        ),
                        (_, CamSignal::NoFace) => format!("   no face          {infer}"),
                        (_, CamSignal::Fault { fault }) => {
                            format!("   {:<17}{infer}", format!("{fault:?}"))
                        }
                        _ => "   -".into(),
                    }
                })
                .collect();
        let signals = Cams(s.0.iter().map(|smp| smp.signal).collect());
        println!(
            "{:7.2} | {} | {:?}",
            t0.elapsed().as_secs_f32(),
            cells.join(" | "),
            crate::scoring::decide(&signals, cfg.switching.decision())
        );
    }
    Ok(())
}

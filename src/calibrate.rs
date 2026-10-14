//! `autocam calibrate` (interactive) and `autocam watch` (live readout).
//!
//! Calibration procedure:
//! 1. Face the centre of the LEFT camera's monitor, press Enter, hold ~3 s.
//! 2. Same for the RIGHT camera's monitor.
//! 3. Optionally look down at the keyboard, press Enter, hold ~3 s.
//!
//! Turn your head, not just your eyes: head orientation is what is measured.
//! Both cameras are sampled in every phase, so each camera learns both the
//! pose it sees when you face its monitor and the pose it sees when you face
//! the other one. That is what absorbs camera mounting angles and normalizes
//! the two cameras' scores onto the same scale.

use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::app::Pipeline;
use crate::config::Config;
use crate::obs::Obs;
use crate::scoring::{Calibration, CalibrationSamples, CamSignal, PhaseSamples, fit_calibration};
use crate::{CamId, PerCam};

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
    let mut out = PhaseSamples::default();
    let mut ticker = tokio::time::interval(cfg.capture.period());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let end = Instant::now() + Duration::from_secs_f32(seconds);
    // Skip the first half-second: the user is still settling.
    tokio::time::sleep(Duration::from_millis(500)).await;
    while Instant::now() < end {
        ticker.tick().await;
        let s = pipeline.sample(obs, cfg, &cal).await;
        out.frames += 1;
        for cam in CamId::ALL {
            if let Some(o) = s.get(cam).observation {
                out.poses.get_mut(cam).push(o.pose);
            }
            if let CamSignal::Fault { fault } = s.get(cam).signal {
                println!("  {cam} camera: {fault:?}");
            }
        }
    }
    println!(
        "  captured {} frames; face seen by left {} / right {}",
        out.frames,
        out.poses.left.len(),
        out.poses.right.len()
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

    println!("\nAutoCam calibration. Turn your HEAD (not just your eyes) for each step.\n");
    wait_enter(&format!(
        "Step 1/{}: face the centre of the LEFT monitor (camera source `{}`), then press Enter and hold still... ",
        if look_down { 3 } else { 2 },
        cfg.cameras.left.source
    ))
    .await?;
    let look_left = collect(&obs, cfg, &mut pipeline, seconds).await?;

    wait_enter(&format!(
        "Step 2/{}: face the centre of the RIGHT monitor (camera source `{}`), then press Enter and hold still... ",
        if look_down { 3 } else { 2 },
        cfg.cameras.right.source
    ))
    .await?;
    let look_right = collect(&obs, cfg, &mut pipeline, seconds).await?;

    let look_down = if look_down {
        wait_enter("Step 3/3: look down at your keyboard, then press Enter and hold still... ")
            .await?;
        Some(collect(&obs, cfg, &mut pipeline, seconds).await?)
    } else {
        None
    };

    let samples = CalibrationSamples {
        look_left,
        look_right,
        look_down,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (cal, report) = fit_calibration(&samples, cfg.switching.confidence_margin, now)
        .context("calibration failed")?;

    println!("\nCalibration result:");
    for cam in CamId::ALL {
        let c = cal.camera(cam);
        println!(
            "  {cam:>5}: target yaw {:+6.1} pitch {:+6.1} | away {} | sigma yaw {:.1} pitch {:.1} | down below {:+.1} | noise {:.1} deg",
            c.target.yaw,
            c.target.pitch,
            c.away.map_or("face lost".to_owned(), |a| format!(
                "yaw {:+6.1} pitch {:+6.1}",
                a.yaw, a.pitch
            )),
            c.sigma_yaw,
            c.sigma_pitch,
            c.down_pitch,
            report.noise.get(cam),
        );
    }
    println!(
        "  predicted scores (L, R): facing left ({:.2}, {:.2}); facing right ({:.2}, {:.2})",
        report.predicted_look_left.0,
        report.predicted_look_left.1,
        report.predicted_look_right.0,
        report.predicted_look_right.1
    );
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
    let cal = cfg.calibration.clone().unwrap_or_default();
    let mut ticker = tokio::time::interval(cfg.capture.period());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let end = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    let params = crate::scoring::DecisionParams {
        min_score: cfg.switching.min_score,
        margin: cfg.switching.confidence_margin,
    };
    println!("time    | left: yaw pitch score  infer | right: yaw pitch score  infer | decision");
    let t0 = Instant::now();
    while end.is_none_or(|e| Instant::now() < e) {
        ticker.tick().await;
        let s = pipeline.sample(&obs, cfg, &cal).await;
        let fmt = |cam: CamId| {
            let smp = s.get(cam);
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
        };
        let signals = PerCam::new(s.left.signal, s.right.signal);
        println!(
            "{:7.2} | {} | {} | {:?}",
            t0.elapsed().as_secs_f32(),
            fmt(CamId::Left),
            fmt(CamId::Right),
            crate::scoring::decide(&signals, params)
        );
    }
    Ok(())
}

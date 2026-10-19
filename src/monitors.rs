//! `autocam monitors`, `autocam map-monitors` and the mouse view of
//! `autocam watch`.

use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::Cams;
use crate::config::{Config, DetectorKind};
use crate::mouse::{self, CursorTarget, Display, MonitorMap};

fn short(device: &str) -> &str {
    device.trim_start_matches(r"\\.\")
}

fn print_displays(cfg: &Config, displays: &[Display], map: Option<&MonitorMap>) {
    for d in displays {
        let role = match map.map(|m| m.cam_for(&d.device)) {
            Some(Some(c)) => format!("-> camera {} `{}`", c.number(), cfg.cam_name(c)),
            Some(None) => "-> keep current camera".into(),
            None => String::new(),
        };
        println!("  {:<10} {role:<26} {}", short(&d.device), d.describe());
    }
}

fn empty_map(cfg: &Config) -> MonitorMap {
    MonitorMap {
        by_cam: vec![Vec::new(); cfg.cameras.len()],
    }
}

/// `autocam monitors`
pub fn list(cfg: &Config) -> Result<()> {
    let displays = mouse::displays()?;
    let map = MonitorMap::resolve(&cfg.cameras, &displays);
    println!("detector: {:?}", cfg.detector);
    println!("displays (physical pixels):");
    print_displays(cfg, &displays, map.as_ref().ok());
    if let Err(e) = &map {
        println!("mapping: {e:#}");
    }
    for (cam, m) in MonitorMap::missing(&cfg.cameras, &displays) {
        println!("warning: camera `{cam}`: configured display `{m}` is not connected");
    }
    let (_, under) = mouse::target(mouse::cursor_pos(), &displays, &empty_map(cfg));
    println!(
        "cursor is on: {}",
        under.as_deref().map_or("unknown", short)
    );
    Ok(())
}

async fn read_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let line = tokio::task::spawn_blocking(|| {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).map(|_| s)
    })
    .await??;
    Ok(line.trim().to_owned())
}

async fn display_under_cursor(cfg: &Config, prompt: &str, displays: &[Display]) -> Result<String> {
    loop {
        read_line(prompt).await?;
        match mouse::target(mouse::cursor_pos(), displays, &empty_map(cfg)).1 {
            Some(d) => {
                println!("  -> {}", short(&d));
                return Ok(short(&d).to_owned());
            }
            None => println!("  could not tell which display the cursor is on; try again"),
        }
    }
}

fn split(list: &str) -> Vec<String> {
    list.split(',')
        .map(|s| short(s.trim()).to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// `autocam map-monitors [--assign NAME=DISPLAY[,DISPLAY]]...`
///
/// With assignments, every camera's displays are replaced by them (cameras
/// not named get none). Without, it asks interactively.
pub async fn map(config_path: &Path, assignments: &[(String, String)]) -> Result<()> {
    let mut cfg = Config::load(config_path)?;
    let displays = mouse::displays()?;
    println!("displays:");
    print_displays(&cfg, &displays, None);

    let mut by_cam: Vec<Vec<String>> = vec![Vec::new(); cfg.cameras.len()];
    if assignments.is_empty() {
        println!(
            "\nFor each prompt, move the mouse onto the display, then press Enter (the window does not need focus for the mouse)."
        );
        for cam in cfg.cam_ids() {
            let d = loop {
                let d = display_under_cursor(
                    &cfg,
                    &format!(
                        "\nDisplay that selects camera {} `{}`: put the mouse there, press Enter... ",
                        cam.number(),
                        cfg.cam_name(cam)
                    ),
                    &displays,
                )
                .await?;
                if !by_cam.iter().flatten().any(|x| *x == d) {
                    break d;
                }
                println!("  that display is already assigned; pick another");
            };
            by_cam[cam.0].push(d);
        }
        let choices: Vec<String> = cfg
            .cameras
            .iter()
            .enumerate()
            .map(|(i, c)| format!("[{}] {}", i + 1, c.name))
            .collect();
        for d in &displays {
            let name = short(&d.device).to_owned();
            if by_cam.iter().flatten().any(|x| *x == name) {
                continue;
            }
            let answer = read_line(&format!(
                "Cursor on {} selects: {} or [k]eep the current camera? [k] ",
                d.describe(),
                choices.join(", ")
            ))
            .await?;
            if let Some(cam) = cfg.find_camera(&answer) {
                by_cam[cam.0].push(name);
            }
        }
    } else {
        for (key, list) in assignments {
            let Some(cam) = cfg.find_camera(key) else {
                let names: Vec<&str> = cfg.cameras.iter().map(|c| c.name.as_str()).collect();
                bail!("no camera `{key}`; cameras are {}", names.join(", "));
            };
            by_cam[cam.0].extend(split(list));
        }
    }

    for (c, mons) in cfg.cameras.iter_mut().zip(by_cam) {
        c.monitors = mons;
    }
    cfg.validate()?;
    let missing = MonitorMap::missing(&cfg.cameras, &displays);
    if !missing.is_empty() {
        let list: Vec<String> = missing.iter().map(|(c, d)| format!("{d} ({c})")).collect();
        bail!("not connected: {}", list.join(", "));
    }
    let map = MonitorMap::resolve(&cfg.cameras, &displays)?;
    for (c, devs) in cfg.cameras.iter().zip(&map.by_cam) {
        if devs.is_empty() {
            println!(
                "note: camera `{}` has no display; only manual selection reaches it",
                c.name
            );
        }
    }
    cfg.detector = DetectorKind::Mouse;
    cfg.save(config_path)?;
    println!("\nmapping:");
    print_displays(&cfg, &displays, Some(&map));
    println!("saved to {} (detector = mouse)", config_path.display());
    Ok(())
}

/// `autocam watch` with the mouse detector: prints every change of display,
/// target and verdict. Needs no OBS.
pub async fn watch(cfg: &Config, seconds: Option<u64>) -> Result<()> {
    let displays = mouse::displays()?;
    let map = MonitorMap::resolve(&cfg.cameras, &displays)?;
    print_displays(cfg, &displays, Some(&map));
    println!("move the mouse between displays (Ctrl+C to stop)");
    let healthy = Cams::filled(cfg.cameras.len(), None);
    let end = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    let mut ticker = tokio::time::interval(cfg.mouse.poll_period());
    let t0 = Instant::now();
    let mut last = None;
    while end.is_none_or(|e| Instant::now() < e) {
        ticker.tick().await;
        let (target, display) = mouse::target(mouse::cursor_pos(), &displays, &map);
        let key = (target, display.clone());
        if last.as_ref() != Some(&key) {
            let what = match target {
                CursorTarget::Camera(c) => format!("camera `{}`", cfg.cam_name(c)),
                CursorTarget::Unmapped => "keep current camera".into(),
                CursorTarget::Unknown => "unknown (hold)".into(),
            };
            println!(
                "{:7.2}s  {:<10} -> {what:<22} {:?}",
                t0.elapsed().as_secs_f32(),
                display.as_deref().map_or("-", short),
                mouse::decide(target, &healthy)
            );
            last = Some(key);
        }
    }
    Ok(())
}

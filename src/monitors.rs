//! `autocam monitors`, `autocam map-monitors` and the mouse view of
//! `autocam watch`.

use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::config::{Config, DetectorKind};
use crate::mouse::{self, CursorTarget, Display, MonitorMap};
use crate::{CamId, PerCam};

fn short(device: &str) -> &str {
    device.trim_start_matches(r"\\.\")
}

fn print_displays(displays: &[Display], map: Option<&MonitorMap>) {
    for d in displays {
        let role = match map.and_then(|m| m.cam_for(&d.device)) {
            Some(CamId::Left) => "-> left camera",
            Some(CamId::Right) => "-> right camera",
            None if map.is_some() => "-> keep current camera",
            None => "",
        };
        println!("  {:<10} {role}  {}", short(&d.device), d.describe());
    }
}

/// `autocam monitors`
pub fn list(cfg: &Config) -> Result<()> {
    let displays = mouse::displays()?;
    let map = MonitorMap::resolve(&cfg.mouse, &displays);
    println!("detector: {:?}", cfg.detector);
    println!("displays (physical pixels):");
    print_displays(&displays, map.as_ref().ok());
    match &map {
        Ok(_) => {}
        Err(e) => println!("mapping: {e:#}"),
    }
    for m in MonitorMap::missing(&cfg.mouse, &displays) {
        println!("warning: configured display `{m}` is not connected");
    }
    let (_, under) = mouse::target(
        mouse::cursor_pos(),
        &displays,
        &MonitorMap {
            left: vec![],
            right: vec![],
        },
    );
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

async fn display_under_cursor(prompt: &str, displays: &[Display]) -> Result<String> {
    loop {
        read_line(prompt).await?;
        let empty = MonitorMap {
            left: vec![],
            right: vec![],
        };
        match mouse::target(mouse::cursor_pos(), displays, &empty).1 {
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

/// `autocam map-monitors [--left A,B] [--right C]`
pub async fn map(config_path: &Path, left: Option<&str>, right: Option<&str>) -> Result<()> {
    let displays = mouse::displays()?;
    println!("displays:");
    print_displays(&displays, None);

    let (left, right) = if left.is_some() || right.is_some() {
        (
            split(left.unwrap_or_default()),
            split(right.unwrap_or_default()),
        )
    } else {
        println!(
            "\nFor each prompt, move the mouse onto the display, then press Enter (the window does not need focus for the mouse)."
        );
        let l = display_under_cursor(
            "\nDisplay that selects the LEFT camera: put the mouse there, press Enter... ",
            &displays,
        )
        .await?;
        let r = loop {
            let r = display_under_cursor(
                "Display that selects the RIGHT camera: put the mouse there, press Enter... ",
                &displays,
            )
            .await?;
            if r != l {
                break r;
            }
            println!("  that display is already the left camera's; pick another");
        };
        let mut left = vec![l];
        let mut right = vec![r];
        for d in &displays {
            let name = short(&d.device).to_owned();
            if left.contains(&name) || right.contains(&name) {
                continue;
            }
            let answer = read_line(&format!(
                "Cursor on {} selects: [l]eft camera, [r]ight camera, or [k]eep the current camera? [k] ",
                d.describe()
            ))
            .await?;
            match answer.to_ascii_lowercase().as_str() {
                "l" | "left" => left.push(name),
                "r" | "right" => right.push(name),
                _ => {}
            }
        }
        (left, right)
    };

    let mut cfg = Config::load(config_path)?;
    cfg.mouse.left_monitors = left;
    cfg.mouse.right_monitors = right;
    let map = MonitorMap::resolve(&cfg.mouse, &displays)?;
    let missing = MonitorMap::missing(&cfg.mouse, &displays);
    if !missing.is_empty() {
        bail!("not connected: {}", missing.join(", "));
    }
    if map.left.is_empty() || map.right.is_empty() {
        bail!("each camera needs at least one display");
    }
    cfg.detector = DetectorKind::Mouse;
    cfg.save(config_path)?;
    println!("\nmapping:");
    print_displays(&displays, Some(&map));
    println!("saved to {} (detector = mouse)", config_path.display());
    Ok(())
}

/// `autocam watch` with the mouse detector: prints every change of display,
/// target and verdict. Needs no OBS.
pub async fn watch(cfg: &Config, seconds: Option<u64>) -> Result<()> {
    let displays = mouse::displays()?;
    let map = MonitorMap::resolve(&cfg.mouse, &displays)?;
    print_displays(&displays, Some(&map));
    println!("move the mouse between displays (Ctrl+C to stop)");
    let healthy = PerCam::new(None, None);
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
                CursorTarget::Camera(c) => format!("{c} camera"),
                CursorTarget::Unmapped => "keep current camera".into(),
                CursorTarget::Unknown => "unknown (hold)".into(),
            };
            println!(
                "{:7.2}s  {:<10} -> {what:<20} {:?}",
                t0.elapsed().as_secs_f32(),
                display.as_deref().map_or("-", short),
                mouse::decide(target, &healthy)
            );
            last = Some(key);
        }
    }
    Ok(())
}

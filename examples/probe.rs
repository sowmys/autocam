//! Prints pixel statistics (never pixels) for each configured camera source,
//! to tell a live picture from a flat placeholder or a frozen frame.
//!
//!     cargo run --release --example probe

use std::time::{Duration, Instant};

use autocam::CamId;
use autocam::config::Config;
use autocam::obs::Obs;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load(&Config::default_path())?;
    let obs = Obs::connect(&cfg.obs).await?;
    let cam: CamId = std::env::args()
        .nth(1)
        .map_or(Ok(CamId::Left), |s| s.parse())
        .map_err(anyhow::Error::msg)?;
    println!("camera: {cam} (`{}`)", cfg.camera(cam).source);
    let mut prev: Option<u64> = None;
    for i in 0..8 {
        let f = obs
            .screenshot(cam, &cfg.camera(cam).source, &cfg.capture)
            .await?;
        let luma: Vec<f32> = f
            .rgb
            .chunks_exact(3)
            .map(|p| 0.299 * f32::from(p[0]) + 0.587 * f32::from(p[1]) + 0.114 * f32::from(p[2]))
            .collect();
        let n = luma.len() as f32;
        let mean = luma.iter().sum::<f32>() / n;
        let std = (luma.iter().map(|l| (l - mean).powi(2)).sum::<f32>() / n).sqrt();
        let (min, max) = luma
            .iter()
            .fold((255f32, 0f32), |(a, b), &l| (a.min(l), b.max(l)));
        // Mean absolute horizontal gradient: ~0 for flat fills, >2 for real scenes.
        let w = f.width as usize;
        let grad: f32 = luma
            .chunks_exact(w)
            .flat_map(|row| row.windows(2).map(|p| (p[1] - p[0]).abs()))
            .sum::<f32>()
            / (n - f.height as f32);
        println!(
            "{i}: {}x{} mean {mean:.1} std {std:.1} min {min:.0} max {max:.0} grad {grad:.2} changed {}",
            f.width,
            f.height,
            prev.map_or("-".into(), |p| (p != f.fingerprint).to_string())
        );
        prev = Some(f.fingerprint);
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(250) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    Ok(())
}

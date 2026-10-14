//! Generates a looping YUV4MPEG2 test clip whose every frame differs (moving
//! noise blocks + a frame counter bar). Used as a stand-in "camera" (an OBS
//! Media Source, i.e. an async video source like a webcam) for the
//! feasibility spike when no physical camera is available.
//!
//!     cargo run --release --example gen_y4m -- out.y4m [seed]

use std::io::{BufWriter, Write};

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: gen_y4m OUT.y4m [seed]");
    let mut seed: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let (w, h, fps, frames) = (320usize, 180usize, 30, 150);
    let mut out = BufWriter::new(std::fs::File::create(path)?);
    writeln!(out, "YUV4MPEG2 W{w} H{h} F{fps}:1 Ip A1:1 C420jpeg")?;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for f in 0..frames {
        out.write_all(b"FRAME\n")?;
        let mut y = vec![0u8; w * h];
        for (i, px) in y.iter_mut().enumerate() {
            let (x, yy) = (i % w, i / w);
            // Moving gradient + per-frame noise: every frame is unique.
            let base = ((x + yy + f * 4) % 200) as u8 + 20;
            *px = base.wrapping_add((rnd() % 9) as u8);
        }
        // Frame-counter bar.
        for yy in 0..8 {
            for x in 0..(f * w / frames) {
                y[yy * w + x] = 235;
            }
        }
        out.write_all(&y)?;
        let chroma: Vec<u8> = (0..(w / 2) * (h / 2)).map(|_| 128).collect();
        out.write_all(&chroma)?;
        out.write_all(&chroma)?;
    }
    out.flush()
}

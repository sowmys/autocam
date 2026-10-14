//! Frame acquisition.
//!
//! OBS owns both webcams; AutoCam only asks OBS for small screenshots of the
//! two camera sources (see [`crate::obs`]). AutoCam never opens a camera
//! device itself, so OBS and AutoCam never compete for one.
//!
//! Frames live only in memory: they are decoded, analysed and dropped.
//! Nothing here writes pixels anywhere, and no `Debug` impl prints them.

use std::time::Instant;

use thiserror::Error;

use crate::CamId;

/// A decoded RGB frame. Pixel data is deliberately excluded from `Debug`.
pub struct Frame {
    pub cam: CamId,
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGB8.
    pub rgb: Vec<u8>,
    pub captured_at: Instant,
    /// Cheap content hash, for staleness detection.
    pub fingerprint: u64,
    /// Mean luma 0-255, for black-frame detection.
    pub mean_luma: f32,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("cam", &self.cam)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fingerprint", &format_args!("{:016x}", self.fingerprint))
            .field("mean_luma", &self.mean_luma)
            .finish_non_exhaustive()
    }
}

impl Frame {
    pub fn from_rgb(cam: CamId, width: u32, height: u32, rgb: Vec<u8>, at: Instant) -> Self {
        let (fingerprint, mean_luma) = fingerprint_and_luma(&rgb);
        Self {
            cam,
            width,
            height,
            rgb,
            captured_at: at,
            fingerprint,
            mean_luma,
        }
    }

    /// Mean absolute difference of a coarse luma grid against another frame,
    /// 0-255. Used by the feasibility spike to tell live video from a frozen
    /// last frame.
    pub fn mean_abs_diff(&self, other: &Frame) -> Option<f32> {
        if self.width != other.width || self.height != other.height {
            return None;
        }
        let (a, b) = (&self.rgb, &other.rgb);
        let mut sum = 0u64;
        let mut n = 0u64;
        for i in (0..a.len().min(b.len())).step_by(3 * 7) {
            sum += u64::from(a[i].abs_diff(b[i]));
            n += 1;
        }
        (n > 0).then(|| sum as f32 / n as f32)
    }
}

/// FNV-1a over every 5th byte, and mean of the green channel as a cheap luma.
fn fingerprint_and_luma(rgb: &[u8]) -> (u64, f32) {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in rgb.iter().step_by(5) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let mut sum = 0u64;
    let mut n = 0u64;
    for px in rgb.chunks_exact(3).step_by(4) {
        // Rec. 601 integer approximation.
        sum += (u64::from(px[0]) * 299 + u64::from(px[1]) * 587 + u64::from(px[2]) * 114) / 1000;
        n += 1;
    }
    (h, if n == 0 { 0.0 } else { sum as f32 / n as f32 })
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("OBS is not connected")]
    NotConnected,
    #[error("OBS request failed: {0}")]
    Obs(String),
    #[error("timed out waiting for OBS")]
    Timeout,
    #[error("could not decode screenshot: {0}")]
    Decode(String),
}

/// Why a camera's signal cannot be used this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalFault {
    /// Screenshot failed (source missing, OBS gone, timeout).
    CaptureFailed,
    /// Frame is (near) black: lens cap, unplugged device, OBS placeholder.
    Black,
    /// Frame has not changed for several samples: frozen or deactivated source.
    Stale,
}

/// Tracks per-camera frame health across ticks.
#[derive(Debug, Default)]
pub struct HealthTracker {
    last_fingerprint: Option<u64>,
    identical_run: u32,
}

impl HealthTracker {
    /// Returns a fault if this frame should not be used.
    pub fn check(
        &mut self,
        frame: &Frame,
        stale_after_identical: u32,
        black_luma_threshold: f32,
    ) -> Option<SignalFault> {
        if self.last_fingerprint == Some(frame.fingerprint) {
            self.identical_run = self.identical_run.saturating_add(1);
        } else {
            self.identical_run = 0;
        }
        self.last_fingerprint = Some(frame.fingerprint);

        if frame.mean_luma < black_luma_threshold {
            Some(SignalFault::Black)
        } else if self.identical_run >= stale_after_identical {
            Some(SignalFault::Stale)
        } else {
            None
        }
    }

    pub fn identical_run(&self) -> u32 {
        self.identical_run
    }
}

/// Decodes the `data:image/...;base64,` URI GetSourceScreenshot returns.
pub fn decode_data_uri(cam: CamId, uri: &str, at: Instant) -> Result<Frame, CaptureError> {
    use base64::Engine as _;
    let payload = uri
        .split_once(";base64,")
        .map(|(_, p)| p)
        .ok_or_else(|| CaptureError::Decode("not a base64 data URI".into()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|e| CaptureError::Decode(e.to_string()))?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| CaptureError::Decode(e.to_string()))?
        .into_rgb8();
    let (w, h) = img.dimensions();
    Ok(Frame::from_rgb(cam, w, h, img.into_raw(), at))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(fill: u8, noise_seed: u8) -> Frame {
        let mut rgb = vec![fill; 64 * 36 * 3];
        rgb[0] = noise_seed;
        Frame::from_rgb(CamId::Left, 64, 36, rgb, Instant::now())
    }

    #[test]
    fn identical_frames_become_stale() {
        let mut h = HealthTracker::default();
        let f = frame(120, 1);
        assert_eq!(h.check(&f, 3, 6.0), None);
        assert_eq!(h.check(&f, 3, 6.0), None);
        assert_eq!(h.check(&f, 3, 6.0), None);
        assert_eq!(h.check(&f, 3, 6.0), Some(SignalFault::Stale));
        // Any change resets it.
        assert_eq!(h.check(&frame(120, 2), 3, 6.0), None);
    }

    #[test]
    fn black_frame_is_fault() {
        let mut h = HealthTracker::default();
        assert_eq!(h.check(&frame(2, 1), 8, 6.0), Some(SignalFault::Black));
        assert_eq!(h.check(&frame(100, 1), 8, 6.0), None);
    }

    #[test]
    fn decodes_png_data_uri() {
        use base64::Engine as _;
        let img = image::RgbImage::from_pixel(8, 4, image::Rgb([200, 100, 50]));
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let uri = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        let f = decode_data_uri(CamId::Right, &uri, Instant::now()).unwrap();
        assert_eq!((f.width, f.height, f.rgb.len()), (8, 4, 8 * 4 * 3));
        assert!(decode_data_uri(CamId::Right, "garbage", Instant::now()).is_err());
    }

    #[test]
    fn debug_never_prints_pixels() {
        let f = frame(77, 1);
        let s = format!("{f:?}");
        assert!(!s.contains("rgb"));
    }

    #[test]
    fn diff_detects_change() {
        let a = frame(100, 1);
        let b = frame(140, 1);
        assert_eq!(a.mean_abs_diff(&a), Some(0.0));
        assert!(a.mean_abs_diff(&b).unwrap() > 30.0);
    }
}

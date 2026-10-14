//! Mouse-position detector: the monitor under the cursor selects the camera.
//!
//! Each camera owns zero or more displays (`[mouse] left_monitors` /
//! `right_monitors`). A display that belongs to neither camera keeps the
//! current camera. With exactly two displays and no configuration, the
//! leftmost display maps to the left camera and the rightmost to the right one.
//!
//! The decision feeds the same switching state machine as face tracking
//! (dwell, minimum interval, manual override), using `[mouse] dwell_ms`.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::capture::SignalFault;
use crate::config::MouseConfig;
use crate::scoring::{Decision, HoldReason};
use crate::{CamId, PerCam};

/// One display, in physical (per-monitor-DPI-aware) virtual-desktop pixels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Display {
    /// GDI device name, e.g. `\\.\DISPLAY2`.
    pub device: String,
    /// Monitor model as reported by the driver, e.g. `Dell P3225QE`.
    pub friendly: String,
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub primary: bool,
}

impl Display {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    pub fn describe(&self) -> String {
        format!(
            "{} ({}{}) {}x{} at ({}, {})",
            self.device,
            self.friendly,
            if self.primary { ", primary" } else { "" },
            self.right - self.left,
            self.bottom - self.top,
            self.left,
            self.top
        )
    }

    fn matches(&self, pattern: &str) -> bool {
        let p = pattern.trim();
        self.device.eq_ignore_ascii_case(p)
            || self
                .device
                .trim_start_matches(r"\\.\")
                .eq_ignore_ascii_case(p.trim_start_matches(r"\\.\"))
    }
}

/// Which camera each display selects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MonitorMap {
    pub left: Vec<String>,
    pub right: Vec<String>,
}

impl MonitorMap {
    /// Resolves the configuration against the connected displays.
    pub fn resolve(cfg: &MouseConfig, displays: &[Display]) -> Result<Self> {
        if cfg.left_monitors.is_empty() && cfg.right_monitors.is_empty() {
            if displays.len() != 2 {
                bail!(
                    "{} displays connected and no monitor mapping configured; run `autocam map-monitors`",
                    displays.len()
                );
            }
            let mut sorted: Vec<&Display> = displays.iter().collect();
            sorted.sort_by_key(|d| (d.left, d.top));
            return Ok(Self {
                left: vec![sorted[0].device.clone()],
                right: vec![sorted[1].device.clone()],
            });
        }
        let pick = |patterns: &[String]| -> Vec<String> {
            displays
                .iter()
                .filter(|d| patterns.iter().any(|p| d.matches(p)))
                .map(|d| d.device.clone())
                .collect()
        };
        let map = Self {
            left: pick(&cfg.left_monitors),
            right: pick(&cfg.right_monitors),
        };
        if let Some(both) = map.left.iter().find(|d| map.right.contains(d)) {
            bail!("display {both} is mapped to both cameras");
        }
        Ok(map)
    }

    /// Configured displays that are not connected (for warnings).
    pub fn missing(cfg: &MouseConfig, displays: &[Display]) -> Vec<String> {
        cfg.left_monitors
            .iter()
            .chain(&cfg.right_monitors)
            .filter(|p| !displays.iter().any(|d| d.matches(p)))
            .cloned()
            .collect()
    }

    pub fn cam_for(&self, device: &str) -> Option<CamId> {
        if self.left.iter().any(|d| d == device) {
            Some(CamId::Left)
        } else if self.right.iter().any(|d| d == device) {
            Some(CamId::Right)
        } else {
            None
        }
    }
}

/// Where the cursor is, relative to the mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorTarget {
    /// Cursor position unavailable (secure desktop, lock screen, UAC prompt).
    Unknown,
    /// On a display that belongs to no camera.
    Unmapped,
    Camera(CamId),
}

/// Per-tick verdict for the switching state machine.
///
/// A camera with a capture fault (black, frozen, missing) is never selected;
/// that and an unknown cursor are *neutral* (missing evidence), while a
/// cursor on an unmapped display is *contrary* (stay put, restart any dwell).
pub fn decide(target: CursorTarget, faults: &PerCam<Option<SignalFault>>) -> Decision {
    match target {
        CursorTarget::Unknown => Decision::Hold {
            reason: HoldReason::CursorUnknown,
        },
        CursorTarget::Unmapped => Decision::Hold {
            reason: HoldReason::UnmappedMonitor,
        },
        CursorTarget::Camera(cam) if faults.get(cam).is_some() => Decision::Hold {
            reason: HoldReason::TargetUnavailable,
        },
        CursorTarget::Camera(cam) => Decision::Prefer {
            cam,
            score: 1.0,
            margin: 1.0,
        },
    }
}

/// Resolves the cursor to a target given the current displays and mapping.
pub fn target(
    cursor: Option<(i32, i32)>,
    displays: &[Display],
    map: &MonitorMap,
) -> (CursorTarget, Option<String>) {
    let Some((x, y)) = cursor else {
        return (CursorTarget::Unknown, None);
    };
    match displays.iter().find(|d| d.contains(x, y)) {
        None => (CursorTarget::Unknown, None),
        Some(d) => (
            map.cam_for(&d.device)
                .map_or(CursorTarget::Unmapped, CursorTarget::Camera),
            Some(d.device.clone()),
        ),
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// Makes cursor and monitor coordinates physical pixels on mixed-DPI setups.
/// Call once at startup; harmless if already set.
#[cfg(windows)]
pub fn enable_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    // SAFETY: plain Win32 call with a constant argument.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

#[cfg(not(windows))]
pub fn enable_dpi_awareness() {}

#[cfg(windows)]
pub fn cursor_pos() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT::default();
    // SAFETY: `p` is a valid out-pointer for the duration of the call.
    unsafe { GetCursorPos(&mut p) }.ok().map(|()| (p.x, p.y))
}

#[cfg(not(windows))]
pub fn cursor_pos() -> Option<(i32, i32)> {
    None
}

#[cfg(windows)]
pub fn displays() -> Result<Vec<Display>> {
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR,
        MONITORINFO, MONITORINFOEXW,
    };
    use windows::core::{BOOL, PCWSTR};

    const MONITORINFOF_PRIMARY: u32 = 1;

    fn wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    unsafe extern "system" fn collect(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<Display>` passed below, alive for
        // the whole EnumDisplayMonitors call.
        let out = unsafe { &mut *(data.0 as *mut Vec<Display>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `info` is a MONITORINFOEXW with cbSize set, as required.
        let ok = unsafe { GetMonitorInfoW(hmon, (&raw mut info).cast::<MONITORINFO>()) };
        if !ok.as_bool() {
            return BOOL(1);
        }
        let device = wide(&info.szDevice);
        let mut dd = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let name: Vec<u16> = device.encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated device name and a correctly sized struct.
        let friendly =
            if unsafe { EnumDisplayDevicesW(PCWSTR(name.as_ptr()), 0, &mut dd, 0) }.as_bool() {
                wide(&dd.DeviceString)
            } else {
                String::new()
            };
        let r = info.monitorInfo.rcMonitor;
        out.push(Display {
            device,
            friendly,
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
        BOOL(1)
    }

    let mut out: Vec<Display> = Vec::new();
    // SAFETY: the callback only touches `out` through the LPARAM, synchronously.
    let ok =
        unsafe { EnumDisplayMonitors(None, None, Some(collect), LPARAM(&raw mut out as isize)) };
    if !ok.as_bool() {
        bail!("EnumDisplayMonitors failed");
    }
    out.sort_by_key(|d| (d.left, d.top));
    Ok(out)
}

#[cfg(not(windows))]
pub fn displays() -> Result<Vec<Display>> {
    bail!("the mouse detector is only available on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(device: &str, left: i32, top: i32, w: i32, h: i32, primary: bool) -> Display {
        Display {
            device: device.into(),
            friendly: "Test".into(),
            left,
            top,
            right: left + w,
            bottom: top + h,
            primary,
        }
    }

    /// The dev rig: two monitors on top, laptop screen below.
    fn rig() -> Vec<Display> {
        vec![
            d(r"\\.\DISPLAY2", -1705, -1800, 3200, 1800, false),
            d(r"\\.\DISPLAY1", 0, 0, 1920, 1200, true),
            d(r"\\.\DISPLAY3", 1495, -1800, 3200, 1800, false),
        ]
    }

    fn cfg(left: &[&str], right: &[&str]) -> MouseConfig {
        MouseConfig {
            left_monitors: left.iter().map(|s| (*s).to_owned()).collect(),
            right_monitors: right.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        }
    }

    const OK: PerCam<Option<SignalFault>> = PerCam {
        left: None,
        right: None,
    };

    #[test]
    fn two_displays_auto_map_left_to_right() {
        let two = vec![
            d(r"\\.\DISPLAY1", 0, 0, 1920, 1080, true),
            d(r"\\.\DISPLAY2", -2560, 0, 2560, 1440, false),
        ];
        let m = MonitorMap::resolve(&MouseConfig::default(), &two).unwrap();
        assert_eq!(m.left, vec![r"\\.\DISPLAY2".to_owned()]);
        assert_eq!(m.right, vec![r"\\.\DISPLAY1".to_owned()]);
    }

    #[test]
    fn three_displays_need_configuration() {
        assert!(MonitorMap::resolve(&MouseConfig::default(), &rig()).is_err());
    }

    #[test]
    fn configured_mapping_with_unmapped_display() {
        // Left Dell -> left camera, laptop screen -> right camera, right Dell -> keep.
        let m = MonitorMap::resolve(&cfg(&["DISPLAY2"], &[r"\\.\DISPLAY1"]), &rig()).unwrap();
        let displays = rig();
        let at = |x, y| target(Some((x, y)), &displays, &m).0;
        assert_eq!(at(-1000, -900), CursorTarget::Camera(CamId::Left));
        assert_eq!(at(500, 500), CursorTarget::Camera(CamId::Right));
        assert_eq!(at(3000, -900), CursorTarget::Unmapped);
        // Gap between displays (cursor cannot be there, but be safe).
        assert_eq!(at(5000, 5000), CursorTarget::Unknown);
        assert_eq!(target(None, &displays, &m).0, CursorTarget::Unknown);
    }

    #[test]
    fn several_displays_per_camera() {
        let m =
            MonitorMap::resolve(&cfg(&["DISPLAY2"], &["DISPLAY1", "DISPLAY3"]), &rig()).unwrap();
        assert_eq!(m.right.len(), 2);
        assert_eq!(m.cam_for(r"\\.\DISPLAY3"), Some(CamId::Right));
    }

    #[test]
    fn display_on_both_cameras_is_rejected() {
        assert!(MonitorMap::resolve(&cfg(&["DISPLAY1"], &["DISPLAY1"]), &rig()).is_err());
    }

    #[test]
    fn missing_configured_display_is_reported() {
        let c = cfg(&["DISPLAY2"], &["DISPLAY9"]);
        assert_eq!(MonitorMap::missing(&c, &rig()), vec!["DISPLAY9".to_owned()]);
    }

    #[test]
    fn edges_are_half_open() {
        let x = d("X", 0, 0, 100, 100, true);
        assert!(x.contains(0, 0) && x.contains(99, 99));
        assert!(!x.contains(100, 50) && !x.contains(50, 100));
    }

    #[test]
    fn decisions() {
        assert!(matches!(
            decide(CursorTarget::Camera(CamId::Left), &OK),
            Decision::Prefer {
                cam: CamId::Left,
                ..
            }
        ));
        // Never switch to a broken camera; neutral so a blip keeps the dwell.
        let faulted = PerCam::new(Some(SignalFault::Black), None);
        let d = decide(CursorTarget::Camera(CamId::Left), &faulted);
        assert_eq!(
            d,
            Decision::Hold {
                reason: HoldReason::TargetUnavailable
            }
        );
        assert!(HoldReason::TargetUnavailable.is_neutral());
        // Unmapped display: stay put and restart any dwell.
        assert!(!HoldReason::UnmappedMonitor.is_neutral());
        assert!(HoldReason::CursorUnknown.is_neutral());
    }

    #[test]
    fn mouse_switching_end_to_end() {
        use crate::switching::{Event, Mode, Switcher};
        use std::time::{Duration, Instant};

        let m = MonitorMap::resolve(&cfg(&["DISPLAY2"], &["DISPLAY1"]), &rig()).unwrap();
        let displays = rig();
        let params = MouseConfig::default().switch_params(&Default::default());
        let mut sw = Switcher::new(params, Mode::Auto);
        let t0 = Instant::now();
        sw.handle(Event::ProgramScene {
            now: t0,
            cam: Some(CamId::Left),
        });
        let tick = Duration::from_millis(100);
        let base = t0 + Duration::from_secs(5);
        let mut run = |from: u32, n: u32, pos: (i32, i32)| -> Vec<u32> {
            (from..from + n)
                .filter(|i| {
                    let dec = decide(target(Some(pos), &displays, &m).0, &OK);
                    sw.handle(Event::Tick {
                        now: base + tick * *i,
                        decision: dec,
                    })
                    .is_some()
                })
                .collect()
        };
        // Cursor crosses the laptop screen for 200 ms: no switch.
        assert!(run(0, 2, (500, 500)).is_empty());
        assert!(run(2, 5, (-1000, -900)).is_empty());
        // Cursor parked on the laptop screen: switch after the 300 ms dwell.
        assert_eq!(run(7, 10, (500, 500)), vec![10]);
        // Right Dell (unmapped): stays on the right camera.
        assert!(run(17, 30, (3000, -900)).is_empty());
    }
}

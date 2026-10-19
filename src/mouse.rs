//! Mouse-position detector: the monitor under the cursor selects the camera.
//!
//! Each camera owns zero or more displays (`monitors` in its `[[cameras]]`
//! entry). A display that belongs to no camera keeps the current camera.
//! When no camera lists any display and there are exactly as many displays
//! as cameras, displays map to cameras in left-to-right order.
//!
//! The decision feeds the same switching state machine as face tracking
//! (dwell, minimum interval, manual override), using `[mouse] dwell_ms`.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::capture::SignalFault;
use crate::config::{CameraConfig, same_display};
use crate::scoring::{Decision, HoldReason};
use crate::{CamId, Cams};

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
        same_display(&self.device, pattern)
    }
}

/// Which displays select which camera: `by_cam[i]` are camera `i`'s devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MonitorMap {
    pub by_cam: Vec<Vec<String>>,
}

impl MonitorMap {
    /// Resolves the cameras' `monitors` against the connected displays.
    pub fn resolve(cameras: &[CameraConfig], displays: &[Display]) -> Result<Self> {
        if cameras.iter().all(|c| c.monitors.is_empty()) {
            if displays.len() != cameras.len() {
                bail!(
                    "{} displays and {} cameras, and no camera has displays assigned; run `autocam map-monitors`",
                    displays.len(),
                    cameras.len()
                );
            }
            let mut sorted: Vec<&Display> = displays.iter().collect();
            sorted.sort_by_key(|d| (d.left, d.top));
            return Ok(Self {
                by_cam: sorted.iter().map(|d| vec![d.device.clone()]).collect(),
            });
        }
        let map = Self {
            by_cam: cameras
                .iter()
                .map(|c| {
                    displays
                        .iter()
                        .filter(|d| c.monitors.iter().any(|p| d.matches(p)))
                        .map(|d| d.device.clone())
                        .collect()
                })
                .collect(),
        };
        for (i, devs) in map.by_cam.iter().enumerate() {
            if let Some(dup) = devs
                .iter()
                .find(|d| map.by_cam[..i].iter().any(|o| o.contains(d)))
            {
                bail!("display {dup} is mapped to more than one camera");
            }
        }
        Ok(map)
    }

    /// Configured displays that are not connected (for warnings), as
    /// (camera name, display).
    pub fn missing(cameras: &[CameraConfig], displays: &[Display]) -> Vec<(String, String)> {
        cameras
            .iter()
            .flat_map(|c| c.monitors.iter().map(move |m| (c, m)))
            .filter(|(_, m)| !displays.iter().any(|d| d.matches(m)))
            .map(|(c, m)| (c.name.clone(), m.clone()))
            .collect()
    }

    pub fn cam_for(&self, device: &str) -> Option<CamId> {
        self.by_cam
            .iter()
            .position(|devs| devs.iter().any(|d| d == device))
            .map(CamId)
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
pub fn decide(target: CursorTarget, faults: &Cams<Option<SignalFault>>) -> Decision {
    match target {
        CursorTarget::Unknown => Decision::Hold {
            reason: HoldReason::CursorUnknown,
        },
        CursorTarget::Unmapped => Decision::Hold {
            reason: HoldReason::UnmappedMonitor,
        },
        CursorTarget::Camera(cam) if faults.0.get(cam.0).is_some_and(Option::is_some) => {
            Decision::Hold {
                reason: HoldReason::TargetUnavailable,
            }
        }
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
    use crate::config::MouseConfig;

    const LEFT: CamId = CamId(0);
    const LAPTOP: CamId = CamId(1);
    const RIGHT: CamId = CamId(2);

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

    fn cams(spec: &[(&str, &[&str])]) -> Vec<CameraConfig> {
        spec.iter()
            .map(|(name, mons)| CameraConfig {
                monitors: mons.iter().map(|s| (*s).to_owned()).collect(),
                ..CameraConfig::named(name)
            })
            .collect()
    }

    fn ok(n: usize) -> Cams<Option<SignalFault>> {
        Cams::filled(n, None)
    }

    #[test]
    fn auto_map_left_to_right_when_counts_match() {
        let two = vec![
            d(r"\\.\DISPLAY1", 0, 0, 1920, 1080, true),
            d(r"\\.\DISPLAY2", -2560, 0, 2560, 1440, false),
        ];
        let m = MonitorMap::resolve(&cams(&[("left", &[]), ("right", &[])]), &two).unwrap();
        assert_eq!(m.cam_for(r"\\.\DISPLAY2"), Some(CamId(0)));
        assert_eq!(m.cam_for(r"\\.\DISPLAY1"), Some(CamId(1)));

        // Three cameras, three displays: left Dell, laptop, right Dell.
        let c = cams(&[("left", &[]), ("laptop", &[]), ("right", &[])]);
        let m = MonitorMap::resolve(&c, &rig()).unwrap();
        assert_eq!(m.cam_for(r"\\.\DISPLAY2"), Some(LEFT));
        assert_eq!(m.cam_for(r"\\.\DISPLAY1"), Some(LAPTOP));
        assert_eq!(m.cam_for(r"\\.\DISPLAY3"), Some(RIGHT));
    }

    #[test]
    fn count_mismatch_needs_configuration() {
        assert!(MonitorMap::resolve(&cams(&[("left", &[]), ("right", &[])]), &rig()).is_err());
    }

    #[test]
    fn three_cameras_one_display_each() {
        let c = cams(&[
            ("left", &["DISPLAY2"]),
            ("laptop", &[r"\\.\DISPLAY1"]),
            ("right", &["display3"]),
        ]);
        let m = MonitorMap::resolve(&c, &rig()).unwrap();
        let displays = rig();
        let at = |x, y| target(Some((x, y)), &displays, &m).0;
        assert_eq!(at(-1000, -900), CursorTarget::Camera(LEFT));
        assert_eq!(at(500, 500), CursorTarget::Camera(LAPTOP));
        assert_eq!(at(3000, -900), CursorTarget::Camera(RIGHT));
        // Gap between displays (cursor cannot be there, but be safe).
        assert_eq!(at(5000, 5000), CursorTarget::Unknown);
        assert_eq!(target(None, &displays, &m).0, CursorTarget::Unknown);
    }

    #[test]
    fn unmapped_display_and_several_per_camera() {
        // Two cameras, right Dell unmapped.
        let c = cams(&[("left", &["DISPLAY2"]), ("right", &["DISPLAY1"])]);
        let m = MonitorMap::resolve(&c, &rig()).unwrap();
        let displays = rig();
        assert_eq!(
            target(Some((3000, -900)), &displays, &m).0,
            CursorTarget::Unmapped
        );
        // Several displays for one camera.
        let c = cams(&[
            ("left", &["DISPLAY2"]),
            ("right", &["DISPLAY1", "DISPLAY3"]),
        ]);
        let m = MonitorMap::resolve(&c, &rig()).unwrap();
        assert_eq!(m.by_cam[1].len(), 2);
        assert_eq!(m.cam_for(r"\\.\DISPLAY3"), Some(CamId(1)));
    }

    #[test]
    fn display_on_two_cameras_is_rejected() {
        let c = cams(&[("left", &["DISPLAY1"]), ("right", &["DISPLAY1"])]);
        assert!(MonitorMap::resolve(&c, &rig()).is_err());
    }

    #[test]
    fn missing_configured_display_is_reported() {
        let c = cams(&[("left", &["DISPLAY2"]), ("right", &["DISPLAY9"])]);
        assert_eq!(
            MonitorMap::missing(&c, &rig()),
            vec![("right".to_owned(), "DISPLAY9".to_owned())]
        );
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
            decide(CursorTarget::Camera(RIGHT), &ok(3)),
            Decision::Prefer { cam: RIGHT, .. }
        ));
        // Never switch to a broken camera; neutral so a blip keeps the dwell.
        let faulted = Cams(vec![None, None, Some(SignalFault::Black)]);
        let d = decide(CursorTarget::Camera(RIGHT), &faulted);
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
    fn mouse_switching_end_to_end_three_cameras() {
        use crate::switching::{Event, Mode, Switcher};
        use std::time::{Duration, Instant};

        let c = cams(&[
            ("left", &["DISPLAY2"]),
            ("laptop", &["DISPLAY1"]),
            ("right", &["DISPLAY3"]),
        ]);
        let m = MonitorMap::resolve(&c, &rig()).unwrap();
        let displays = rig();
        let params = MouseConfig::default().switch_params(&Default::default());
        let mut sw = Switcher::new(params, Mode::Auto);
        let t0 = Instant::now();
        sw.handle(Event::ProgramScene {
            now: t0,
            cam: Some(LEFT),
        });
        let tick = Duration::from_millis(100);
        let base = t0 + Duration::from_secs(5);
        let healthy = ok(3);
        let mut run = |from: u32, n: u32, pos: (i32, i32)| -> Vec<(u32, CamId)> {
            (from..from + n)
                .filter_map(|i| {
                    let dec = decide(target(Some(pos), &displays, &m).0, &healthy);
                    sw.handle(Event::Tick {
                        now: base + tick * i,
                        decision: dec,
                    })
                    .map(|cmd| (i, cmd.to))
                })
                .collect()
        };
        // Cursor crosses the laptop screen for 200 ms: no switch.
        assert!(run(0, 2, (500, 500)).is_empty());
        assert!(run(2, 5, (-1000, -900)).is_empty());
        // Parked on the laptop screen: switch after the 300 ms dwell.
        assert_eq!(run(7, 10, (500, 500)), vec![(10, LAPTOP)]);
        // Straight on to the right Dell: the dwell is met at tick 20, but the
        // 1.5 s minimum interval since the switch at tick 10 holds it to 25.
        assert_eq!(run(17, 20, (3000, -900)), vec![(25, RIGHT)]);
    }
}

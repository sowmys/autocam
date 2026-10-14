# Measured results

**Machine:** Lenovo laptop, Intel Core i7-1270P (16 logical CPUs), 31.6 GB RAM,
Windows 11 Pro 10.0.26300. Rust 1.99.0 MSVC, release build. OBS 32.2.2 with
obs-websocket 5.7.4. Date: 2026-10-14.

**Provenance labels**

- **MEASURED**: observed on this machine.
- **SYNTHETIC**: real AutoCam code on simulated head-pose signals.
- **PENDING**: needs the acceptance test on a two-webcam rig.

History:

1. **First session (one camera):** the Intel IPU "Integrated Camera" was sending a
   static "camera off" placeholder (mean luma 137.4, mean gradient 0.21, 0% frame
   change over 90 s), so real-camera results were invalid.
2. **Second session:** a Dell Webcam WB7022 was added and the laptop camera's
   shutter opened. The real two-webcam spike (§1c) and resource measurements
   (§2a) come from this session.
3. **Still blocking face tracking:** the dev network's web filter blocks
   `files.pythonhosted.org`, the source of `libmediapipe.dll` (the TLS handshake
   is cut, while `pypi.org` is reachable). Fix: an allow-list entry, or run
   `scripts/fetch-mediapipe.ps1` from another network.

## 1. Feasibility spike

### 1a. Async video proxies: MEASURED

Two OBS Media Sources looping generated 320x180 30 FPS video (every frame
unique) stand in for two webcams. 60 s per run, Program rotated every 3 s.
Reports: `results/spike-proxy-no-keepalive.json` and
`results/spike-proxy-keepalive.json`.

| Keepalive | Camera | Program shows | Samples | Fail | Fresh | OBS `active` | p50 / p95 latency |
|---|---|---|---|---|---|---|---|
| off | left | own scene | 84 | 0 | 100% | 100% | 7.8 / 13.4 ms |
| off | left | other camera's scene | 84 | 0 | 100% | 17% | 8.2 / 11.5 ms |
| off | right | own scene | 84 | 0 | 100% | 100% | 6.6 / 11.4 ms |
| off | right | other camera's scene | 84 | 0 | 100% | 0% | 6.9 / 13.8 ms |
| on | left | own scene | 84 | 0 | 100% | 100% | 8.9 / 19.0 ms |
| on | left | other camera's scene | 84 | 0 | 100% | **100%** | 8.0 / 18.7 ms |
| on | right | own scene | 84 | 0 | 100% | 100% | 7.3 / 20.1 ms |
| on | right | other camera's scene | 84 | 0 | 100% | **100%** | 7.2 / 18.0 ms |

- Achieved 4.0 FPS per camera.
- `SetCurrentProgramScene` acknowledged in p50 12.7 ms / p95 27.3 ms with
  keepalive off, and p50 18.7 ms / p95 134 ms with keepalive on (n = 21 each).
- The ~17% `active` with keepalive off is OBS's 300 ms fade, during which both
  scenes render.

**Conclusion:** the screenshot path itself (latency, rate, failures) is
comfortably within budget. Media Sources keep decoding when not shown, so they
cannot reproduce webcam freezing (the negative-control setting does not apply to
them). The keepalive mechanism verifiably keeps the non-Program camera
`active`.

### 1b. Real laptop webcam, single camera, before the privacy placeholder was found: SUPERSEDED by 1c

First run, before the camera was found to be sending a placeholder. Program
rotated through LeftCam, RightCam and an empty scene. Both scenes contained the
same camera input, so "off Program" means the empty scene was on Program.

| State | Samples | Fail | Fresh | Longest identical run | p50 / p95 latency |
|---|---|---|---|---|---|
| on Program | 180 | 0 | 94.4% | 5 | 10.9 / 14.8 ms |
| off Program | 60 | 0 | **23.3%** | 17 | 11.1 / 14.9 ms |

- OBS's log shows no device re-initialization during this run (the device stayed
  open with *Deactivate when not showing* off).
- Not-shown frames nevertheless went stale, about one fresh frame per 3 s window.
- 1c contradicts this run, so it was most likely the camera's privacy
  placeholder, which changes only rarely, rather than OBS. Later runs were
  clearly placeholder: `results/spike-real-camera-placeholder-INVALID.json`.

### 1c. Two real webcams, A/B on keepalive: MEASURED (acceptance A2: pass)

Left = Dell Webcam WB7022, right = Intel IPU Integrated Camera (privacy shutter
open). *Deactivate when not showing* off. 60 s per run, Program rotated every
3 s through LeftCam, RightCam and an empty scene. Reports:
`results/spike-real-no-keepalive.json`, `results/spike-real-keepalive.json`.

| Keepalive | Camera | Program shows | Samples | Fail | Fresh | OBS `active` | p50 / p95 latency |
|---|---|---|---|---|---|---|---|
| off | left (Dell) | own scene | 84 | 0 | 100% | 100% | 6.2 / 8.6 ms |
| off | left (Dell) | other camera's scene | 84 | 0 | **100%** | 17% | 6.7 / 15.9 ms |
| off | right (laptop) | own scene | 84 | 0 | 100% | 100% | 6.2 / 15.7 ms |
| off | right (laptop) | other camera's scene | 84 | 0 | **97.6%** | 0% | 6.0 / 8.1 ms |
| on | left (Dell) | own scene | 84 | 0 | 100% | 100% | 6.8 / 16.1 ms |
| on | left (Dell) | other camera's scene | 84 | 0 | **100%** | 100% | 6.8 / 11.5 ms |
| on | right (laptop) | own scene | 84 | 0 | 100% | 100% | 6.2 / 11.7 ms |
| on | right (laptop) | other camera's scene | 84 | 0 | **100%** | 100% | 6.5 / 15.0 ms |

- Achieved 4.01 FPS per camera.
- Mean luma 113–186 and mean frame-to-frame difference 2–3 (sensor noise),
  so these are live pictures.
- `SetCurrentProgramScene` acknowledged in p50 11.8–13.0 ms and p95 17–40 ms.

**Conclusion:** OBS GetSourceScreenshot returns fresh frames for the camera that
is not on Program, with or without keepalive, on both webcams. The OBS design is
confirmed and the native fallback is not needed. Keepalive stays in
`obs-setup` as cheap insurance against drivers that throttle hidden sources.

Negative control (*Deactivate when not showing* on, with keepalive on): stale
runs of up to 17–18 frames (about 4.5 s) and 34% freshness, caught by the
spike. With keepalive both cameras stay active, so most of this comes from the
devices re-opening after the setting change. It still shows that the spike, and
the runtime's 8-identical-frames rule, detect frozen video.

## 2. Resource use

### 2a. Capture and decision loop, no face model: MEASURED

`autocam run --no-face`: 2 cameras × 4 FPS, OBS screenshots, JPEG decode,
health checks, decisions, HTTP status. 60 s windows, CPU from Windows process
CPU time.

| Metric | Two real webcams | Two-proxy rig |
|---|---|---|
| AutoCam CPU | **0.11% of machine** (1.7% of one core) | 0.21% (3.3% of one core) |
| AutoCam memory | **21.9 MB** working set, 7.6 MB private | 23.2 MB, 7.7 MB private |
| OBS CPU with AutoCam sampling | 0.94% of machine | 1.08% |
| OBS CPU, AutoCam stopped | 0.71%, so sampling adds **+0.23%** | 0.81%, so +0.27% |
| Tick (capture + decode + decide) | p50 8.0 ms, p95 11.1 ms | p50 11.9 ms, p95 19.8 ms |
| Screenshot round trip p95 | 10.3 ms (left), 10.7 ms (right) | 16.6 / 18.7 ms |
| Capture faults | 0 / 0 | 0 / 0 |

### 2b. With MediaPipe inference: PENDING

The model is MediaPipe face_landmarker float16 (BlazeFace short-range detector
plus 478-point mesh), 2 cameras × 4 FPS = 8 inferences/s. Expect a few ms per
inference on this CPU class (XNNPACK), roughly 1% or less of the machine in total,
and about 60–100 MB of memory for the library and two landmarkers. **These are
estimates, not measurements.** Measure with acceptance test G
(`cams.*.inference_ms`, `resources.*`).

## 3. Switching quality, face mode: SYNTHETIC

`autocam simulate`, 10 simulated hours per row, seed 1 (stress rows seed 2).

Script:

- A user alternates between monitors (3–30 s), looks down (0.8–8 s), glances
  at the gap (0.3–1.2 s) and leaves (2–12 s).
- Monitors are at ±22° head yaw. Camera mounting biases are +4° and −6°.
- Head turns take 350 ms.
- Per-sample Gaussian pose noise, plus gross outliers (15–40°) and detection
  dropouts at the listed rates.
- The face is lost beyond 60° relative yaw.

Each run calibrates from 3 s per phase with the same noise. **Latency** runs
from the start of the head turn to the switch command (OBS acknowledgement adds
about 15–30 ms, §1a). A **false switch** is an auto switch to a camera the user
is not facing, including any switch while looking down, at the gap, or away.

Default settings (dwell 800 ms, min interval 1500 ms, margin 0.25, min score
0.45, max gap 500 ms, min votes 3). `results/simulate.json`,
`results/simulate-stress.json`:

| Noise | Outliers | Dropouts | Labels | Switches | Correct | **False/h** | Missed | Latency p50 / p95 / max |
|---|---|---|---|---|---|---|---|---|
| 1.5° | 2% | 5% | 2880 | 1553 | 1553 | **0.00** | 0 | 1250 / 2000 / 4000 ms |
| 3° | 2% | 5% | 2880 | 1553 | 1553 | **0.00** | 0 | 1250 / 2000 / 4000 ms |
| 5° | 2% | 5% | 2880 | 1551 | 1551 | **0.00** | 0 | 1250 / 2250 / 5000 ms |
| 8° | 2% | 5% | 2880 | 1553 | 1553 | **0.00** | 1 | 1250 / 2500 / 5250 ms |
| 3° | 8% | 30% | 2889 | 1587 | 1587 | **0.00** | 12 | 1500 / 3500 / 5250 ms |
| 5° | 8% | 30% | 2889 | 1587 | 1587 | **0.00** | 16 | 1500 / 3750 / 5250 ms |

Strict dwell (`--strict`, max gap 0, any dropout restarts the dwell), for comparison:

| Noise | Outliers | Dropouts | False/h | Missed | Latency p50 / p95 |
|---|---|---|---|---|---|
| 3° | 2% | 5% | 0.00 | 2 | 1250 / 3000 ms |
| 8° | 2% | 5% | 0.20 | 9 | 1500 / 3250 ms |
| 3° | 8% | 30% | 1.50 | 509 | 2500 / 5000 ms |
| 5° | 8% | 30% | 1.40 | 534 | 2500 / 5000 ms |

The 1250 ms median is consistent with an 800 ms dwell, a 350 ms head turn and
4 FPS sampling. The high p95 and max values come from outliers resetting the
dwell, plus the minimum interval when the user turns back quickly.

**Real-camera false-switch rate and latency: PENDING** (acceptance test G, with
ground-truth hotkeys).

## 4. Functional checks against live OBS: MEASURED

| Check | Result |
|---|---|
| WebSocket auth + connect | OK. OBS 32.2.2 / obs-websocket 5.7.4 |
| `obs-setup` creates scenes, input, device selection | OK. Found "Integrated Camera", 640x360 NV12 @ 30 |
| `obs-setup` from one-camera simulation to two real cameras | Bug found and fixed. The right camera reused the shared simulation input, and the old full-size item stayed in RightCam. Now the right camera gets its own `AutoCam Right` input, and existing cross-scene items are converted to hidden keepalives. Verified: Dell → LeftCam, Integrated → RightCam |
| Manual select via HTTP | 204. Program switched, mode → Manual |
| Back to Auto with no face | Holds (`hold/no_face`). No switch over 5 s |
| POST without `X-AutoCam` header | 403 |
| `Host: attacker.example:8787` | 403 |
| Listening sockets | `127.0.0.1:8787` only |
| OBS closed while running | Logged `OBS is shutting down`. `/status` showed `obs_connected:false`. Retried with backoff 0.5 → 10 s (since reduced to a 5 s cap) |
| OBS restarted | Reconnected automatically (25 s after OBS start command, about 14 s of which was OBS startup). Frames resumed, 0 faults |
| Camera stuck black after settings toggle | Detected (luma 0). Fixed by `autocam reactivate` (Windows shows the device in use again) |

## 5. Code quality: MEASURED

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | exit 0 (`results/fmt.txt`) |
| `cargo clippy --all-targets --release -- -D warnings` | exit 0 (`results/clippy.txt`) |
| `cargo test --release` | 62 passed, 0 failed (`results/test.txt`) |

Unit-test coverage by required area:

- **Hysteresis:** `switching::tests::*`
  - dwell boundary at exactly 800 ms
  - contrary evidence resets
  - short and long tracking dropouts
  - strict mode, minimum votes, minimum interval
  - holds never switch
- **Calibration:** `scoring::tests::calibration_*`
  - angled cameras, face lost when looking away
  - no face, no separation, weak separation
  - TOML round-trip, calibrated decisions
- **Disconnected cameras:** `decide_no_face_and_faults`,
  `decide_one_camera_disconnected_other_frontal`, `capture::tests::*` (stale and
  black detection), `switch_failure_restores_previous`, `simulate::noisy_disconnected_like_signals_do_not_flap`
- **Ambiguous detection:** `decide_holds_when_ambiguous`,
  `decide_holds_when_both_low`, `decide_holds_when_looking_down_even_if_one_is_frontal`
- **Manual override:** `manual_override_wins_and_sticks`,
  `back_to_auto_needs_fresh_dwell`, `manual_select_respects_following_min_interval`,
  `external_program_change_is_adopted`
- **Pose geometry:** `face::tests::*` (synthetic rotated faces, off-centre correction)
- **Mouse detector:** `mouse::tests::*`
  - two-display auto-mapping; three displays require configuration
  - several displays per camera; a display on both cameras is rejected
  - missing displays reported; half-open display edges
  - faulted target / unmapped / unknown verdicts
  - end to end with the switcher: flick ignored, 300 ms dwell, unmapped holds
- **Security:** `config::tests::rejects_non_loopback_endpoints`,
  `debug_redacts_password`, `diagnostics::tests::routes_and_hardening`,
  `capture::tests::debug_never_prints_pixels`

## 6. Mouse detector, live: MEASURED

Rig: OBS 32.2.2, Dell Webcam WB7022 → LeftCam, Intel IPU laptop camera → RightCam.
Three displays (physical pixels, per-monitor-DPI-aware):

| Display | Model | Size | Position | Mapping |
|---|---|---|---|---|
| DISPLAY2 | Dell P3225QE | 3840x2160 | (-2046, -2163) | left camera |
| DISPLAY1 | OLED laptop panel, primary | 3840x2400 | (0, 0) | right camera |
| DISPLAY3 | Dell P3225QE | 3840x2160 | (1794, -2160) | keep current camera |

`autocam run` with defaults (poll 10 Hz, dwell 300 ms, min interval 1500 ms,
health check 1 FPS). The cursor was moved by script (`SetCursorPos`), and times
are UTC from the script and the AutoCam log.

| Step | Cursor | Expected | Observed |
|---|---|---|---|
| 1 | left Dell | stay on LeftCam | no switch |
| 2 | laptop screen (22:10:30.151) | switch right | switch at 22:10:30.493: **342 ms** after the move (decision 314 ms + OBS ack 17 ms) |
| 3 | left Dell for 150 ms, then back | no switch | no switch |
| 4 | right Dell (unmapped) for 3 s | stay on RightCam | no switch |
| 5 | left Dell (22:10:38.829) | switch left | switch at 22:10:39.274: **445 ms** (decision 387 ms + ack 11 ms) |
| 6–7 | laptop for 400 ms, then left Dell | at most one switch | no switch (cursor left before dwell + poll completed) |
| A–B | laptop for 700 ms (22:11:09.620), then left Dell | right, then left ≥ 1.5 s later | right at 22:11:10.040 (420 ms), left at 22:11:11.596: **1.556 s** apart (min interval enforced) |

- OBS acknowledgement: p50 16.8 ms. Tick: p95 11.3 ms (including the 1 FPS health screenshots).
- AutoCam CPU about 0.1% of machine, memory 22 MB. Camera faults: 0 during the test runs.

**Camera fault caught live.** In the first attempt, the laptop camera had
frozen inside OBS: a real image (mean gradient 2.57), byte-identical across
frames. AutoCam logged `right camera unavailable (Stale); will not switch to it`
and refused every switch to it, as designed. `autocam reactivate` did not
recover it. Restarting OBS did. This is recorded as a troubleshooting entry in
SETUP.md.

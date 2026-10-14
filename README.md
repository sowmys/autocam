# AutoCam

Native Windows 11 tool (Rust 2024, Tokio) that switches OBS between two webcam
scenes, **LeftCam** and **RightCam**, based on the monitor you are using. OBS
sends the result to Microsoft Teams via the OBS Virtual Camera.

Two detectors decide which camera is live (`detector` in the config, or
`--detector` on the command line):

| Detector | Signal | Status |
|---|---|---|
| **`mouse`** (default) | The display under the mouse cursor selects that display's camera | **Working, verified live** |
| `face` | Head orientation from MediaPipe face landmarks, per-camera calibration | Built and unit-tested; live verification pending (MediaPipe runtime blocked on the dev network) |

```
 webcam L ─┐                         ┌── GetSourceScreenshot (camera health / face frames, in memory)
 webcam R ─┤  OBS (owns both cams) ──┤
           │  scenes LeftCam/RightCam └── SetCurrentProgramScene ◄── AutoCam ◄── mouse cursor display
           └─► Program ─► Virtual Camera ─► Teams                          └── (or) head pose
```

- OBS owns both webcams. AutoCam never opens a camera device.
- **Mouse mode:** each camera owns one or more displays. A display that belongs
  to neither camera keeps the current camera. A switch needs the cursor to stay
  300 ms on the other camera's display, with at least 1500 ms between switches.
  Cameras get a 1 FPS health check, so AutoCam never switches to a black or
  frozen camera.
- **Face mode:** a stable opposing selection for 800 ms, a confidence margin,
  and holds when you look down, when tracking fails, or when the signal is
  ambiguous.
- Manual selection, an auto/manual toggle, global hotkeys, and a localhost
  status/control endpoint.
- Frames are never saved, sent or logged. Network endpoints are loopback only,
  and OBS WebSocket uses authentication.

## Status

| Area | State |
|---|---|
| Build, `cargo fmt`, `cargo clippy -D warnings`, 62 unit tests | **passing** ([results](docs/results/)) |
| Mouse detector end to end (OBS, real webcams, 3 displays) | **verified live**: switch 342–445 ms after the cursor moves, flicks ignored, minimum interval enforced |
| OBS transport, reconnect, manual override, HTTP controls | **verified live** against OBS 32.2.2 |
| Feasibility spike (screenshot freshness for the inactive camera) | **passes on two real webcams** ([RESULTS §1c](docs/RESULTS.md#1-feasibility-spike)) |
| CPU/memory on real webcams | **measured**: AutoCam about 0.1% of machine, 22 MB; OBS +0.2% |
| Face detector live verification | **pending**: the dev network's web filter blocks `files.pythonhosted.org`, where `libmediapipe.dll` comes from |

Measured numbers and their provenance are in [docs/RESULTS.md](docs/RESULTS.md).

## Build (Windows 11, x64)

Prerequisites:

1. **Rust** stable, MSVC toolchain (1.85 or later; tested with 1.99):
   ```powershell
   winget install Rustlang.Rustup
   rustup default stable-x86_64-pc-windows-msvc
   ```
2. **MSVC build tools**: Visual Studio 2022 or Build Tools 2022 with the
   "Desktop development with C++" workload (provides `link.exe` and the Windows SDK).
3. **OBS Studio 30+**, which includes obs-websocket 5 (tested with 32.2.2):
   `winget install OBSProject.OBSStudio`

Build and test:

```powershell
cargo build --release
cargo test --release
cargo clippy --all-targets --release -- -D warnings
```

The mouse detector needs nothing else. For the face detector only:

```powershell
# MediaPipe face model (3.7 MB, SHA-256 pinned) -> models\face_landmarker.task
.\scripts\fetch-model.ps1
# libmediapipe.dll (34 MB, from the official PyPI wheel, SHA-256 pinned).
# The mediapipe crate downloads this on first run. If that fails (corporate TLS
# inspection or a blocked files.pythonhosted.org), run:
.\scripts\fetch-mediapipe.ps1
#   or set MEDIAPIPE_LIB=C:\path\to\libmediapipe.dll
```

The binary is `target\release\autocam.exe`. The default config file is
`%APPDATA%\AutoCam\config.toml`.

## Quick start (mouse mode)

```powershell
autocam obs-setup --left-device "Dell Webcam" --right-device "Integrated Camera"
autocam map-monitors        # put the mouse on each camera's display, press Enter
autocam watch               # optional: shows display -> camera as you move the mouse
autocam run
```

## Usage

| Command | Purpose |
|---|---|
| `autocam obs-setup --left-device "<name>" --right-device "<name>"` | Create LeftCam/RightCam scenes, camera inputs and keepalive items in OBS |
| `autocam monitors` | List displays, the mouse mapping, and the display under the cursor |
| `autocam map-monitors [--left DISPLAY2] [--right DISPLAY1,DISPLAY3]` | Choose which displays select which camera (interactive without flags) |
| `autocam watch` | Mouse: live display → camera. Face: live yaw/pitch/score per camera |
| `autocam run` | Automatic switching (the default command) |
| `autocam --detector face run` | Use head orientation instead of the mouse for this run |
| `autocam calibrate` | Face mode: face the left monitor, then the right, then the keyboard |
| `autocam run --no-face` | Face mode without MediaPipe: manual-only |
| `autocam spike --seconds 60 [--negative-control] [--without-keepalive]` | Feasibility spike: are both cameras' screenshots fresh at 4 FPS? |
| `autocam reactivate` | Re-open camera devices stuck black or frozen in OBS |
| `autocam simulate --hours 10` | Face mode: false-switch rate and latency on synthetic head-pose signals |
| `autocam config` | Show the effective configuration (password redacted) |

While running:

- **Hotkeys:** Ctrl+Alt+Left / Ctrl+Alt+Right selects a camera and enters Manual mode.
  Ctrl+Alt+A toggles Auto/Manual. Ctrl+Alt+Shift+Left / Right / Down labels
  ground truth for measurements.
- **Console:** `l` `r` `a` `m` `t` `1` `2` `0` `s` `q`.
- **HTTP (127.0.0.1:8787):** `GET /status` returns JSON and `GET /` returns a one-line summary.
  `POST /select/{left,right}`, `/mode/{auto,manual,toggle}` and
  `/label/{left,right,none}` need the header `X-AutoCam: 1`.

Guides:

- [OBS + Teams setup](docs/SETUP.md)
- [Manual acceptance test](docs/ACCEPTANCE.md)
- [Design and spike decision](docs/DESIGN.md)
- [Measured results](docs/RESULTS.md)

## Layout

```
src/
  mouse.rs        Mouse detector: displays, cursor, display->camera map, per-tick decision
  monitors.rs     monitors / map-monitors / mouse watch commands
  obs.rs          OBS WebSocket transport: auth, timeouts, reconnect/backoff, screenshots, scenes, keepalive
  capture.rs      Frame type (no pixels in Debug), data-URI decode, stale/black detection
  face.rs         MediaPipe FaceLandmarker per camera; head pose from transform matrix or landmarks
  scoring.rs      Calibration fit, per-camera Gaussian frontality score, per-tick decision
  switching.rs    Dwell / min-interval / manual-override state machine (clock-free, shared by both detectors)
  config.rs       TOML config, loopback validation, redacted password
  diagnostics.rs  Status, latency stats, ground-truth evaluator, CPU/RSS sampler, loopback HTTP
  control.rs      Console commands and Windows global hotkeys
  app.rs          run loop: detector ticks, vision threads, health checks, reconnect, controls
  spike.rs        Feasibility spike
  calibrate.rs    Face calibrate / watch
  simulate.rs     Face-mode synthetic simulation
examples/         probe (pixel stats, no pixels), gen_y4m + proxy_rig (spike without cameras)
scripts/          fetch-model.ps1, fetch-mediapipe.ps1 (hash-pinned downloads)
```

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

New to OBS or AutoCam? Start with [How it works](#how-it-works).

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

## How it works

OBS does all the video work. AutoCam does no video work at all: it only
decides which camera should be live and tells OBS. Think of OBS as a TV
studio's vision mixer, and AutoCam as the director calling "cut to camera 2".

```
 Dell webcam ───┐
                ├──►  OBS  ──►  OBS Virtual Camera  ──►  Teams
 Laptop camera ─┘      ▲        (looks like a webcam)
                       │
                       │  "show LeftCam" / "show RightCam"
                       │
                    AutoCam  ◄── where is your mouse?
```

### OBS: the video mixer

- **Owns both webcams.** OBS is the only program that opens them. That's why
  Teams must never select a physical webcam: two apps fighting over one camera
  is a common cause of black or frozen video.
- **Holds two scenes.** **LeftCam** shows the left webcam full-screen and
  **RightCam** shows the right one. A scene is a ready-made picture layout.
- **Has one Program output,** the scene currently "on air". Switching cameras
  just means changing which scene is on Program, and OBS does that instantly
  (or with a short fade).
- **Sends Program to Teams.** The **Virtual Camera** makes OBS's output appear
  to Windows as a webcam called "OBS Virtual Camera". Teams treats it like any
  camera and has no idea there are two real ones behind it.
- **Can be remote-controlled.** OBS's built-in **WebSocket server** (port 4455,
  password-protected) lets another program on your PC ask it to do things.
  That's how AutoCam talks to it.

Without AutoCam, OBS works fine: you'd click LeftCam or RightCam yourself
whenever you wanted to switch.

### AutoCam: the automatic director

AutoCam replaces your clicking. It runs a small loop:

1. **Every 100 ms, it checks where the mouse is** and which display that is.
2. **It looks up which camera that display belongs to,** for example left
   monitor → LeftCam, laptop screen → RightCam, another monitor → no change.
3. **It waits until it's sure.** The mouse must stay 300 ms on the other
   camera's display, and switches are at least 1.5 s apart, so moving the
   mouse across a screen doesn't make the picture flicker.
4. **It tells OBS to switch** by sending "set Program to RightCam" over the
   WebSocket.
5. **Once a second it checks both cameras' health.** It asks OBS for a small
   snapshot of each camera, held only in memory and never saved. If a camera
   is black or frozen, AutoCam refuses to switch to it.

It also handles the extras:

- **Manual override:** Ctrl+Alt+Left / Ctrl+Alt+Right to choose a camera
  yourself, Ctrl+Alt+A to resume automatic switching.
- **Reconnecting** if OBS restarts.
- **A status page** at `http://127.0.0.1:8787/status` that only your own PC can
  reach.

### Who does what

| Task | OBS | AutoCam |
|---|---|---|
| Opens the webcams | ✓ | never |
| Produces the video Teams sees | ✓ | |
| Holds the LeftCam / RightCam scenes | ✓ | (creates them once via `obs-setup`) |
| Decides *which* camera should be live | | ✓ (mouse position, or head direction) |
| Performs the switch | ✓ | asks OBS to |
| Watches for broken cameras | | ✓ |

That's why the start order matters:

1. **OBS first,** because it owns the cameras and the output.
2. **Then Start Virtual Camera,** so Teams has something to show.
3. **Then AutoCam,** so the picture follows your mouse.

If AutoCam stops, Teams keeps working: the picture just stops switching
automatically. If OBS stops, Teams loses the picture.

### Face mode: who evaluates which camera you are facing?

**AutoCam does, on your own PC**, using Google's open-source MediaPipe face
model running inside the AutoCam process. OBS doesn't evaluate anything; it
only supplies the pictures. No cloud service is involved. Face mode changes
only step 1 of the loop: instead of "where's the mouse?" it asks "which camera
are you facing?". Everything after that is the same.

1. **Snapshots from OBS.** Four times a second, AutoCam asks OBS for a small
   snapshot (480×270) from **each** camera. The snapshots stay in memory and
   are discarded right after analysis. They are never saved, sent anywhere or
   logged.
2. **Face landmarks (MediaPipe).** Each snapshot goes through MediaPipe's *Face
   Landmarker*, a small pretrained model (`face_landmarker.task`, 3.7 MB) that
   runs locally on the CPU. It finds your face and places 478 points on it:
   eye corners, nose tip, chin and so on.
3. **Head direction.** From those points, AutoCam works out which way your head
   is turned (left/right and up/down) **as seen from that camera**. If you face
   the left camera straight on, it sees a frontal face while the right camera
   sees you turned away.
4. **Calibration.** Cameras are rarely mounted perfectly. A one-time
   `autocam calibrate` has you look at each monitor for a few seconds, so
   AutoCam learns what "facing this monitor" looks like from each camera,
   including its mounting angle.
5. **Scores and decision.** Each camera gets a 0–1 score for how closely what it
   sees matches its "facing me" pose. AutoCam switches only when one camera
   clearly wins (by a margin) and keeps winning for 0.8 s. It holds the current
   camera when:
   - you look down at the keyboard,
   - no face is found,
   - both scores are close, for example when you look at the gap between the
     monitors.
6. **The switch.** As in mouse mode, AutoCam then tells OBS "show LeftCam" or
   "show RightCam".

So the chain is: **OBS** provides the pictures → **MediaPipe, inside AutoCam**
finds your face → **AutoCam** turns that into "which camera you're facing" and
decides → **OBS** switches.

Face mode measures where your **head** points, not your eyes. Glancing at the
other monitor without turning your head won't switch, which is intentional.

For the technical design (pipelines, scoring formulas, state machine), see
[docs/DESIGN.md](docs/DESIGN.md).

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

## Run it (mouse mode)

All commands run from the repo root in a normal PowerShell window. AutoCam is
`.\target\release\autocam.exe` after `cargo build --release`.

### One-time setup

1. **Enable OBS WebSocket.** With **OBS closed**, run:
   ```powershell
   .\scripts\enable-obs-websocket.ps1
   ```
   This turns on OBS's WebSocket server with a random password and saves the
   password for AutoCam (it is never printed).
2. **Start OBS** from the Start menu. Cancel the first-run wizard if it appears.
3. **Create the camera scenes.** Use part of each webcam's name as OBS lists it
   (OBS → Add Source → Video Capture Device shows the names):
   ```powershell
   .\target\release\autocam.exe obs-setup --left-device "Dell" --right-device "Integrated"
   ```
   OBS now has scenes **LeftCam** and **RightCam**, one camera each. The
   command also sets OBS's picture size (canvas and output) to **1920×1080 at
   30 fps**, sets each webcam to 1920×1080 (or the closest widescreen size it
   supports), and makes each camera fill the frame. Add `--resolution 1280x720`
   for a lighter load. It's safe to re-run at any time. If the Virtual
   Camera is on, it is briefly stopped and restarted.
4. **Map your displays to cameras:**
   ```powershell
   .\target\release\autocam.exe map-monitors
   ```
   Put the mouse on the display that should select the left camera and press
   Enter, then the same for the right camera. Any other display can select a
   camera or keep the current one. Check the result with
   `.\target\release\autocam.exe monitors`.

### Every time

Start things in this order:

1. **OBS:** start it from the Start menu and leave it running. It owns both
   webcams, and Teams reads its output.
2. **Virtual camera:** in OBS, click **Start Virtual Camera** (Controls dock,
   bottom right).
3. **AutoCam:** in a PowerShell window, run:
   ```powershell
   .\target\release\autocam.exe run
   ```
   Leave the window open. It logs a line on every switch. Press `q` (or
   Ctrl+C) to stop.
4. **Teams:** open Teams, go to **Settings → Devices → Camera**, and choose **OBS
   Virtual Camera**. Do this once; Teams remembers it. Never pick a physical
   webcam in Teams, because OBS must be the only app using the cameras.

### Test it

- **Camera preview:** with Teams' **Settings → Devices** open, move the mouse
  to the other camera's display. The preview switches within about half a
  second. On a display mapped to neither camera, nothing changes.
- **Meeting:** in Teams, open **Calendar → Meet now → Start meeting** with
  the camera on. To see what others see, join the same meeting from your phone.
  Your own self-view is mirrored; others see you the right way round.
- **Manual override:** **Ctrl+Alt+Left** / **Ctrl+Alt+Right** picks a camera and
  pauses automatic switching. **Ctrl+Alt+A** resumes it.

If something is off:

| Symptom | Fix |
|---|---|
| Teams shows black or a frozen OBS image | Start the Virtual Camera *before* opening Teams, or restart Teams |
| Your image fills only part of the Teams picture, or looks blurry | Re-run step 3 (`obs-setup`). It sets OBS to 1920×1080 and fits the cameras to the frame |
| AutoCam logs `camera unavailable (…); will not switch to it` | That webcam is black or frozen. Run `autocam reactivate`; if it stays frozen, restart OBS |
| `N displays connected and no monitor mapping configured` | Run `autocam map-monitors` |
| `connecting to OBS … failed` | OBS isn't running, or step 1 of the one-time setup was skipped |

More detail is in [docs/SETUP.md](docs/SETUP.md).

## Usage

| Command | Purpose |
|---|---|
| `autocam obs-setup --left-device "<name>" --right-device "<name>" [--resolution 1920x1080]` | Create LeftCam/RightCam scenes and camera inputs in OBS; set the canvas and camera resolution; fit the cameras to the frame; add keepalive items |
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

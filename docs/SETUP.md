# OBS and Microsoft Teams setup

## 1. Place the cameras

- Mount one webcam on (or directly beside) each monitor, pointing at where your
  head is when you face that monitor. Head-on is ideal; up to roughly 30° off
  is absorbed by calibration.
- Light your face from the front. Strong backlight (a window behind you)
  degrades face tracking.
- AutoCam supports 2 to 9 cameras, one per monitor (several monitors can share
  a camera).
- Plug the cameras into separate USB ports or controllers if you can. Two or
  three 1080p MJPEG streams on one hub can exceed its bandwidth.

## 2. Enable OBS WebSocket (with authentication)

**Scripted (recommended):** close OBS, then from the repo root run

```powershell
.\scripts\enable-obs-websocket.ps1
```

It enables the server on port 4455 with authentication, generates a random
password (or reuses an existing one), and stores it in
`%APPDATA%\AutoCam\config.toml` without printing it. It also skips OBS's
first-run wizard. Then start OBS and continue with step 3.

**By hand:** OBS → **Tools → WebSocket Server Settings**:

1. Tick **Enable WebSocket server**, keep port **4455**.
2. Tick **Enable Authentication** and set a long random password (or click *Generate*).
3. Apply.

Give AutoCam the password, preferably through an environment variable, which
is never written to disk by AutoCam:

```powershell
[Environment]::SetEnvironmentVariable('AUTOCAM_OBS_PASSWORD', '<password>', 'User')
```

Alternatively put it in `%APPDATA%\AutoCam\config.toml`:

```toml
[obs]
password = "<password>"
```

That file lives in your user profile. Restrict it to your account if others
use the machine. AutoCam never logs the password, and `autocam config` prints it
redacted.

**Network exposure.** obs-websocket listens on all interfaces and has no
bind-address option. AutoCam itself only connects to `127.0.0.1` and refuses
non-loopback hosts. To keep other machines out, rely on the password and add a
Windows Firewall rule (run in an elevated PowerShell):

```powershell
New-NetFirewallRule -DisplayName "OBS WebSocket: local only" -Direction Inbound `
  -Protocol TCP -LocalPort 4455 -RemoteAddress Any -Action Block -Profile Any
```

Loopback traffic is not filtered by Windows Firewall, so AutoCam keeps working.

## 3. Create the scenes

Automatic (recommended). List device names in OBS (Add Source → Video Capture
Device), then give each camera a short name and part of its device name, once
per camera, in order (left to right is a good convention):

```powershell
autocam obs-setup --camera left=dell --camera laptop=integrated --camera right=logitech
# optional: --resolution 1280x720   (default 1920x1080)
```

Cameras are numbered in that order (1 = `left`, 2 = `laptop`, 3 = `right`);
the numbers are what Ctrl+Alt+1/2/3 select. Names may use lowercase letters,
digits, `-` and `_`. Re-running with a different list replaces the cameras;
existing cameras keep their OBS names and display mapping.

It sets:

- **OBS canvas and output:** `--resolution` (default 1920×1080) at 30 fps. This
  is the picture Teams receives. A fresh OBS profile whose first-run wizard
  was skipped runs at 640×480. If the Virtual Camera is on, it is stopped and
  restarted around this change, because OBS refuses it while an output is
  active.
- **Camera resolution:** AutoCam asks each webcam which modes it supports and
  picks `--resolution` if available, else the largest widescreen (16:9) mode
  that fits, else the largest mode that fits. Device defaults are often as low
  as 640×360.
- **Fit to frame:** each camera is scaled to fill its scene (aspect ratio kept,
  centred), like OBS's *Transform → Fit to screen*. Without this, a camera
  smaller than the canvas sits in the top-left corner.

The command is safe to re-run. For each camera it creates a scene `<Name>Cam`
holding an input `AutoCam <Name>` full-frame, plus every *other* camera as a
**keepalive** item. With three cameras:

| Scene | Contains |
|---|---|
| `LeftCam` | `AutoCam Left` (full frame) + keepalives of `AutoCam Laptop`, `AutoCam Right` |
| `LaptopCam` | `AutoCam Laptop` (full frame) + keepalives of `AutoCam Left`, `AutoCam Right` |
| `RightCam` | `AutoCam Right` (full frame) + keepalives of `AutoCam Left`, `AutoCam Laptop` |

It also sets **Deactivate when not showing = off** on every camera. This is the
setting that matters: it keeps the camera you are *not* currently shown on
running, so AutoCam keeps getting fresh frames from it.

A **keepalive** item is another camera at 1% scale, positioned far off the
canvas and locked. It is never visible in the output. It keeps every camera
rendered on Program at all times. On the tested webcams this was not needed
(see [DESIGN.md](DESIGN.md#feasibility-spike)). It is insurance for cameras or
drivers that throttle hidden sources, and is safe to keep. If you remove it,
re-run `autocam spike` to confirm all cameras stay fresh.

Manual equivalent:

1. OBS → Settings → **Video**: Base (Canvas) and Output (Scaled) Resolution
   **1920x1080**, Common FPS **30**.
2. Create one scene per camera, e.g. `LeftCam`, `LaptopCam`, `RightCam`.
3. In each scene, add a Video Capture Device for its camera (`AutoCam Left`,
   ...). Properties: Resolution/FPS Type **Custom**, resolution 1920x1080 (or
   1280x720), untick **Deactivate when not showing**. Then right-click it →
   Transform → **Fit to screen**.
4. Optional keepalives: in each scene, *Add Existing* every other camera input.
   Transform → Edit Transform: position (-20000, -20000), size 1%. Lock it.
5. Optional: Scene Transitions → **Cut** for instant switches, or **Fade 150–300 ms**
   for softer ones.
6. Describe the cameras in `config.toml`, in order:
   ```toml
   [[cameras]]
   name = "left"
   source = "AutoCam Left"   # OBS input name
   scene = "LeftCam"         # OBS scene name

   [[cameras]]
   name = "laptop"
   source = "AutoCam Laptop"
   scene = "LaptopCam"
   ```

## 4. Verify

```powershell
autocam spike --seconds 60    # must print VERDICT: RELIABLE
```

## 5a. Mouse mode (default): map displays to cameras

```powershell
autocam monitors       # lists displays as DISPLAY1, DISPLAY2, ... with size and position
autocam map-monitors   # interactive: put the mouse on each camera's display, press Enter
```

`map-monitors` asks, for each camera in turn, for the display that selects it.
For any further display it asks which camera, or **keep** (the cursor there
leaves the current camera live). Non-interactive form:

```powershell
autocam map-monitors --assign left=DISPLAY2 --assign laptop=DISPLAY1 --assign right=DISPLAY3
# several displays for one camera: --assign right=DISPLAY3,DISPLAY4
```

The result goes into each camera's entry in `config.toml`:

```toml
detector = "mouse"

[[cameras]]
name = "left"
source = "AutoCam Left"
scene = "LeftCam"
monitors = ["DISPLAY2"]       # several allowed, e.g. ["DISPLAY2", "DISPLAY4"]

# ... one [[cameras]] entry per camera ...

[mouse]
poll_hz = 10.0                # cursor polls per second
dwell_ms = 300                # cursor must stay this long before a switch
health_check_fps = 1.0        # camera health screenshots; 0 disables
```

If no camera has displays assigned and there are exactly as many displays as
cameras, displays map to cameras left to right. Display numbers can change after
docking or re-plugging monitors. AutoCam re-reads the layout every 5 s and logs
a warning if a mapped display is gone; re-run `map-monitors` if the numbers
moved. `[switching] min_switch_interval_ms` (1500) also applies in mouse mode.

```powershell
autocam watch     # shows display -> camera as you move the mouse; no OBS needed
autocam run
```

## 5b. Face mode (optional)

Needs `scripts\fetch-model.ps1` and `libmediapipe.dll` (see README).

```powershell
autocam --detector face calibrate    # face each camera's monitor in turn, then the keyboard
autocam --detector face watch        # sanity check: scores follow your head
autocam --detector face run          # or set detector = "face" in config.toml
```

## 6. Send OBS to Teams

1. OBS → **Start Virtual Camera** (Controls dock). Optionally set *Output type:
   Program* in the virtual camera settings.
2. Teams → Settings → **Devices** → Camera → **OBS Virtual Camera**.
3. In a meeting, the camera menu should show the OBS output. Move the mouse to
   another camera's display and the picture follows within about half a
   second (mouse mode), or turn your head (face mode, about a second).

Notes:

- Teams mirrors your *self-view* only; others see it unmirrored.
- If Teams shows a black or static OBS Virtual Camera image, start the virtual
  camera *before* opening Teams, or restart Teams.
- Never select a physical webcam in Teams. OBS must be the only app opening the
  cameras, or one of them will fail to start.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `connecting to OBS ...: authentication` | Password mismatch; check `AUTOCAM_OBS_PASSWORD` / config |
| Status shows `stale` for a camera, or the log says `camera unavailable (Stale); will not switch to it` | Frozen frames. Try `autocam reactivate`. If it stays frozen, **restart OBS**: the Intel IPU laptop camera was seen stuck inside OBS (device open, one frozen frame) where only an OBS restart helped. Also check that *Deactivate when not showing* is off |
| `… displays and … cameras, and no camera has displays assigned` | Run `autocam map-monitors` |
| Ctrl+Alt+number types a character instead (keyboard layouts where AltGr = Ctrl+Alt) | Use Ctrl+Alt+Left / Right, the console, or set `hotkeys = false` under `[diagnostics]` |
| OBS uses more CPU than expected | Each camera costs OBS decode time, roughly in proportion to its resolution (measured: OBS about 0.9% of the machine with two cameras at 640×480/640×360, about 4.5–6% with three at 1080p). Use `obs-setup … --resolution 1280x720` for a lighter load |
| Mouse on a display does nothing | That display is unmapped (keeps the current camera) or its number changed: run `autocam monitors` |
| Status shows `black` for a camera | Device not delivering video: privacy shutter, camera F-key, another app holding it. Then run `autocam reactivate` |
| Your image fills only part of the Teams picture (e.g. the top-left quarter), or looks blurry | OBS canvas is small (640×480 on a fresh profile) or cameras are at a low default resolution. Re-run `autocam obs-setup …`; it sets 1920×1080 and fits the cameras to the frame |
| `changing the OBS canvas` error from `obs-setup` | OBS is recording or streaming; stop that and re-run |
| Camera shows a grey "camera off" image | Laptop privacy shutter or camera kill switch (e.g. Lenovo F8/F9). Open or toggle it |
| `MediaPipe face tracking unavailable` | Run `scripts\fetch-mediapipe.ps1`, or set `MEDIAPIPE_LIB` |
| Hotkey not registered | Another app (e.g. Intel graphics screen rotation) owns Ctrl+Alt+Arrow; use the console or HTTP instead |
| Mouse mode switches too eagerly / too slowly | Adjust `[mouse] dwell_ms` (300 default; 0 = next poll) and `[switching] min_switch_interval_ms` |
| Face mode switches feel slow | Lower `[switching] dwell_ms` (minimum sensible about 500) or `min_switch_interval_ms`; re-run `autocam simulate` to see the false-switch cost |
| Face mode switches when glancing | Raise `confidence_margin` (0.3–0.4) or re-calibrate turning your head fully to each monitor |

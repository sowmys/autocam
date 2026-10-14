# OBS and Microsoft Teams setup

## 1. Place the cameras

- Mount one webcam on (or directly beside) each monitor, pointing at where your
  head is when you face that monitor. Head-on is ideal; up to roughly 30° off
  is absorbed by calibration.
- Light your face from the front. Strong backlight (a window behind you)
  degrades face tracking.
- Plug both cameras into separate USB ports or controllers if you can.
  Two 1080p MJPEG streams on one hub can exceed its bandwidth.

## 2. Enable OBS WebSocket (with authentication)

OBS → **Tools → WebSocket Server Settings**:

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
Device), then:

```powershell
autocam obs-setup --left-device "Logitech BRIO" --right-device "Integrated Camera"
```

This creates:

| Scene | Contains |
|---|---|
| `LeftCam` | `AutoCam Left` (full frame) + `AutoCam Right` as a **keepalive** item |
| `RightCam` | `AutoCam Right` (full frame) + `AutoCam Left` as a **keepalive** item |

It also sets **Deactivate when not showing = off** on both cameras. This is the
setting that matters: it keeps the camera you are *not* currently shown on
running, so AutoCam keeps getting fresh frames from it.

A **keepalive** item is the other camera at 1% scale, positioned far off the
canvas and locked. It is never visible in the output. It keeps both cameras
rendered on Program at all times. On the tested webcams this was not needed
(see [DESIGN.md](DESIGN.md#feasibility-spike)). It is insurance for cameras or
drivers that throttle hidden sources, and is safe to keep. If you remove it,
re-run `autocam spike` to confirm both cameras stay fresh.

Manual equivalent:

1. Create scenes `LeftCam` and `RightCam`.
2. In `LeftCam`, add a Video Capture Device `AutoCam Left` (left camera).
   Properties: resolution 1280x720 or 1920x1080, untick **Deactivate when not showing**.
3. In `RightCam`, add `AutoCam Right` (right camera), same settings.
4. In `LeftCam`, *Add Existing* `AutoCam Right`. Transform → Edit Transform:
   position (-20000, -20000), size 1%. Lock it.
5. In `RightCam`, do the same with `AutoCam Left`.
6. Optional: Scene Transitions → **Cut** for instant switches, or **Fade 150–300 ms**
   for softer ones.

Names can differ. Set them in `config.toml` under `[cameras.left]` / `[cameras.right]`
(`source` = input name, `scene` = scene name).

## 4. Verify

```powershell
autocam spike --seconds 60    # must print VERDICT: RELIABLE
```

## 5a. Mouse mode (default): map displays to cameras

```powershell
autocam monitors       # lists displays as DISPLAY1, DISPLAY2, ... with size and position
autocam map-monitors   # interactive: put the mouse on each camera's display, press Enter
```

`map-monitors` asks for the display that selects the **left** camera, then the
**right** camera. For any further display it asks left, right, or **keep**
(the cursor there leaves the current camera live). The result goes to
`config.toml`:

```toml
detector = "mouse"

[mouse]
left_monitors = ["DISPLAY2"]
right_monitors = ["DISPLAY1"]   # several allowed, e.g. ["DISPLAY1", "DISPLAY3"]
poll_hz = 10.0                  # cursor polls per second
dwell_ms = 300                  # cursor must stay this long before a switch
health_check_fps = 1.0          # camera health screenshots; 0 disables
```

With exactly two displays and no mapping, the leftmost display selects the
left camera and the rightmost the right camera. Display numbers can change after
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
autocam --detector face calibrate    # face left monitor, right monitor, keyboard
autocam --detector face watch        # sanity check: scores follow your head
autocam --detector face run          # or set detector = "face" in config.toml
```

## 6. Send OBS to Teams

1. OBS → **Start Virtual Camera** (Controls dock). Optionally set *Output type:
   Program* in the virtual camera settings.
2. Teams → Settings → **Devices** → Camera → **OBS Virtual Camera**.
3. In a meeting, the camera menu should show the OBS output. Move the mouse to
   the other camera's display and the picture follows within about half a
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
| `N displays connected and no monitor mapping configured` | Run `autocam map-monitors` |
| Mouse on a display does nothing | That display is unmapped (keeps the current camera) or its number changed: run `autocam monitors` |
| Status shows `black` for a camera | Device not delivering video: privacy shutter, camera F-key, another app holding it. Then run `autocam reactivate` |
| Camera shows a grey "camera off" image | Laptop privacy shutter or camera kill switch (e.g. Lenovo F8/F9). Open or toggle it |
| `MediaPipe face tracking unavailable` | Run `scripts\fetch-mediapipe.ps1`, or set `MEDIAPIPE_LIB` |
| Hotkey not registered | Another app (e.g. Intel graphics screen rotation) owns Ctrl+Alt+Arrow; use the console or HTTP instead |
| Mouse mode switches too eagerly / too slowly | Adjust `[mouse] dwell_ms` (300 default; 0 = next poll) and `[switching] min_switch_interval_ms` |
| Face mode switches feel slow | Lower `[switching] dwell_ms` (minimum sensible about 500) or `min_switch_interval_ms`; re-run `autocam simulate` to see the false-switch cost |
| Face mode switches when glancing | Raise `confidence_margin` (0.3–0.4) or re-calibrate turning your head fully to each monitor |

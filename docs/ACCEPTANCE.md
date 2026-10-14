# Manual acceptance test

Run on the target machine with two physical webcams, OBS configured per
[SETUP.md](SETUP.md), and `autocam.exe` built in release mode. Each step has a
pass criterion. Record results in the table at the end.

Before you start, have:

- OBS open, WebSocket enabled with a password, Virtual Camera started.
- A second terminal for HTTP checks (`curl.exe` ships with Windows).
- Teams open in a test meeting, or the Teams *camera preview* in Settings → Devices.

## A. Setup and feasibility

| # | Step | Pass |
|---|---|---|
| A1 | `autocam obs-setup --left-device "<L>" --right-device "<R>"` | Prints "OBS scenes ready". OBS shows `LeftCam` and `RightCam`, each with its camera full-frame and a locked keepalive item off-canvas |
| A2 | `autocam spike --seconds 60 --negative-control --json spike.json` | Main phase `reliable: true`, VERDICT RELIABLE; for both cameras **other cam scene** fresh% ≥ 95, fail 0, p95 ≤ 150 ms. Negative control *not* reliable (shows that staleness is detected) |
| A3 | `autocam spike --seconds 60 --without-keepalive` (keepalive items are disabled for the run, then restored) | Informational. If **other cam scene** fresh% < 95 here but A2 passes, keep the keepalive items; your cameras need them |

If A2 fails *with* keepalive, the OBS-screenshot design is not viable on this
hardware. Stop and see [DESIGN.md, native fallback](DESIGN.md#native-capture-fallback-not-implemented).
(Dev rig, 2026-10-14, Dell WB7022 + Intel IPU: A2 and A3 both passed. See
[RESULTS.md §1c](RESULTS.md#1c-two-real-webcams-ab-on-keepalive-measured-acceptance-a2-pass).)

## M. Mouse mode (default detector)

Map the displays first: `autocam map-monitors`. Then start `autocam run` and
keep its console visible.

| # | Step | Pass |
|---|---|---|
| M1 | `autocam monitors` | Lists every display with size and position. Mapping shows each camera's displays; others `-> keep current camera` |
| M2 | `autocam watch`, move the mouse across all displays | Each display prints the expected camera (or "keep current camera") |
| M3 | In `run`: park the cursor on a display of the *other* camera | Switches once, about 0.3–0.5 s later. Log `switch … (Auto) … cursor on \\.\DISPLAYn` |
| M4 | Flick the cursor onto the other camera's display for under 200 ms and back (10 times) | **No** switch |
| M5 | Park the cursor on an unmapped display for 10 s | **No** switch |
| M6 | Move to the other camera's display for about 0.7 s, then straight back | Two switches at most, the second ≥ 1.5 s after the first |
| M7 | Cover or disable one camera (shutter / unplug), then put the cursor on its display | Log `… camera unavailable (…); will not switch to it` within a few seconds. **No** switch to it. After it recovers (`autocam reactivate`, or restart OBS if it stays frozen) switching resumes |
| M8 | Lock the screen (Win+L) for 10 s and unlock | No switch while locked. No crash |

Dev rig, 2026-10-14 (left Dell → LeftCam, laptop screen → RightCam, right Dell
unmapped): M1–M7 passed, with M3–M6 scripted. M7 occurred naturally when the
laptop camera froze, and AutoCam refused to switch to it. See
[RESULTS.md §6](RESULTS.md#6-mouse-detector-live-measured).

Sections B and C below are for **face mode** only (`--detector face`).
Sections D–F apply to both detectors.

## B. Calibration (face mode)

| # | Step | Pass |
|---|---|---|
| B1 | `autocam calibrate`. Face the left monitor centre, then the right, then the keyboard. Turn your **head** | No error. Predicted scores: facing left (L ≥ 0.8, R ≤ 0.2); facing right (R ≥ 0.8, L ≤ 0.2). No warnings, or only "weak separation" warnings you accept |
| B2 | `autocam watch --seconds 30`, slowly turn left ↔ right | The facing camera's score rises above 0.8 and the other's falls below 0.2. Decision reads `Prefer` for the faced camera, and `Hold` (ambiguous / low_score) half-way |
| B3 | In `watch`, look at the keyboard | Decision `Hold { looking_down }` |

## C. Automatic switching (face mode)

Start `autocam --detector face run` and keep its console visible.

| # | Step | Pass |
|---|---|---|
| C1 | Face left monitor 5 s, then turn to right monitor and hold | Teams/OBS switches to RightCam once, about 1–1.5 s after the turn. Log line `switch left -> right (Auto) decision=…ms obs_ack=…ms` |
| C2 | Turn back to left and hold | Switches to LeftCam once |
| C3 | From left, glance at right monitor for **½ s** and back (repeat 10 times) | **No** switch |
| C4 | Turn to right, then back to left after 1 s | At most one switch each way. Second switch never sooner than 1.5 s after the first |
| C5 | Look down at the keyboard for 10 s, from either monitor | **No** switch |
| C6 | Look at the gap between monitors for 10 s | **No** switch |
| C7 | Walk out of frame for 20 s, come back facing the *other* monitor | No switch while away. Switches about 1 s after you are back and facing it |
| C8 | Cover one camera's lens with your hand for 10 s while facing *that* monitor | No switch to the other camera. Status shows `black` or `noface` for the covered camera |
| C9 | Another person walks behind you | No switch (the nearest face is tracked) |

## D. Manual control and modes

| # | Step | Pass |
|---|---|---|
| D1 | Press Ctrl+Alt+Right while using the left monitor (cursor there / facing it) | Immediate switch to RightCam. Log `manual select right`. Mode Manual |
| D2 | Keep using the left monitor for 10 s | Stays on RightCam (manual override sticks) |
| D3 | Ctrl+Alt+A (back to Auto) | Returns to LeftCam within about 0.5 s (mouse) or 1 s (face) |
| D4 | `curl.exe -X POST -H "X-AutoCam: 1" http://127.0.0.1:8787/select/left` | HTTP 204, switch, mode Manual |
| D5 | `curl.exe -X POST http://127.0.0.1:8787/select/right` (no header) | HTTP 403, no switch |
| D6 | Click a scene in OBS by hand while in Auto | AutoCam logs "program scene changed in OBS … adopting" and does not snap back for at least 1.5 s |
| D7 | Quit with `q`, restart `autocam run` | Mode from before quitting is restored (stored in config.toml) |

## E. Robustness

| # | Step | Pass |
|---|---|---|
| E1 | Close OBS while AutoCam runs | AutoCam logs reconnect attempts with growing delay. No crash. `/status` shows `obs_connected: false` |
| E2 | Start OBS again | Reconnects automatically (within about 5 s of OBS's WebSocket coming up). Switching resumes |
| E3 | Unplug one camera for 30 s, then replug | Status shows a fault for it. No switch *to* it. After replug (OBS may need `autocam reactivate`) it recovers |
| E4 | Wrong password in config | Clear authentication error, retries, no password in logs |

## F. Privacy and security

| # | Step | Pass |
|---|---|---|
| F1 | `Get-NetTCPConnection -OwningProcess (Get-Process autocam).Id -State Listen` | Only `127.0.0.1:8787` |
| F2 | `curl.exe -H "Host: evil.example:8787" http://127.0.0.1:8787/status` | HTTP 403 |
| F3 | Search the AutoCam folder and `%APPDATA%\AutoCam` for new image files after a 10-minute run | None. Only `config.toml` |
| F4 | `autocam --log debug run` for 1 minute, inspect the output | Numbers only (poses, scores, timings); no base64, no pixel data, no password |

## G. Measurements (fill in)

Mouse mode needs no labels: read `switches.decision_latency_ms` (cursor
settles → switch command, about 300–400 ms) and `switches.obs_ack_ms` from
`/status` after normal work. Its false-switch rate is, by construction, the
rate of parking the cursor ≥ 300 ms on the other camera's display without
meaning to.

**Face mode:** run 15 minutes of normal work with `autocam --detector face run`, labelling ground truth as you
go: **Ctrl+Alt+Shift+Left / Right** when you turn to that monitor and keep
working there, **Ctrl+Alt+Shift+Down** when you look at keyboard, notes or phone.
Then read `curl.exe http://127.0.0.1:8787/status`:

| Metric | Where | Target | Measured |
|---|---|---|---|
| AutoCam CPU (whole machine) | `resources.cpu_percent` (or Task Manager) | < 3% | |
| OBS CPU increase vs. AutoCam stopped | Task Manager, OBS process | < 2% | |
| AutoCam memory | `resources.rss_mb` | < 150 MB | |
| Inference time per frame | `cams.*.inference_ms.p95` | < 30 ms | |
| Switch latency (label → switch) | `eval.latency_ms.p50 / p95` | p50 ≤ 1.5 s, p95 ≤ 2.5 s | |
| False switches per hour | `eval.false_switches_per_hour` | < 1 | |
| Missed switches | `eval.missed_switches` | 0–1 | |
| Reversals (unlabelled false-switch proxy) | `switches.reversals` | 0 | |
| OBS switch acknowledgement | `switches.obs_ack_ms.p95` | < 100 ms | |

Sign-off, mouse mode: A2, M1–M8, D1–D7, E1–E2 and F1–F4 pass.
Sign-off, face mode: additionally B1, C1–C9, and G meets targets.

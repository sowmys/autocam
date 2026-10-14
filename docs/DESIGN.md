# AutoCam design

Two interchangeable detectors produce a per-tick `Decision` (prefer a camera,
or hold for a reason). Everything after that is shared: the switching state
machine, OBS transport, controls and diagnostics. `detector = "mouse"` is the
default. `"face"` is kept for later.

## Mouse pipeline (default)

```
every 100 ms (mouse.poll_hz = 10)
 ├─ mouse.rs      GetCursorPos → display under the cursor (per-monitor-DPI-aware,
 │                physical pixels) → camera via [mouse] left_monitors / right_monitors
 │                cursor on own camera / unmapped display / unknown → Decision
 ├─ every 1 s     camera health: GetSourceScreenshot both cameras → black/stale → fault
 │                (a faulted camera is never selected)
 ├─ switching.rs  dwell = mouse.dwell_ms (300), min_votes = 1, min interval 1500 ms
 └─ obs.rs        SetCurrentProgramScene(LeftCam | RightCam)
```

| Cursor | Verdict | Effect on the dwell |
|---|---|---|
| on a display of the current camera | Prefer(current) | contrary: resets |
| on a display of the other camera, camera healthy | Prefer(other) | accumulates; switch after 300 ms |
| on a display of the other camera, camera faulted | Hold(TargetUnavailable) | neutral: never switches |
| on an unmapped display | Hold(UnmappedMonitor) | contrary: keep current camera |
| position unavailable (lock screen, UAC prompt) | Hold(CursorUnknown) | neutral |

The display layout is re-read every 5 s, so docking and unplugging are
followed. With exactly two displays and no mapping, leftmost → left camera and
rightmost → right camera. Otherwise `autocam map-monitors` writes the mapping.
The process declares per-monitor-v2 DPI awareness at startup, so cursor and
display rectangles share one physical coordinate space on mixed-DPI setups.

## Face pipeline

```
every 250 ms (4 FPS, tokio interval, missed ticks skipped)
 ├─ obs.rs        GetSourceScreenshot(left source), GetSourceScreenshot(right source)   [concurrent]
 │                480x270 JPEG q80, encoded by OBS → base64 data URI
 ├─ capture.rs    decode in memory → RGB8; fingerprint + mean luma
 │                HealthTracker: black (luma < 6) / stale (8 identical frames) → Fault
 ├─ face.rs       one MediaPipe FaceLandmarker per camera, each on its own OS thread
 │                (video mode, monotonic timestamps, up to 2 faces → nearest)
 │                head pose = transform matrix (preferred) or landmark-plane fallback,
 │                relative to the face→camera line (moving sideways ≠ turning)
 ├─ scoring.rs    per-camera Gaussian score from calibrated target pose → decide()
 ├─ switching.rs  dwell / min interval / manual override → SwitchCommand
 └─ obs.rs        SetCurrentProgramScene(LeftCam | RightCam)
```

The frame is dropped as soon as the vision thread returns. Pixels are never
written, sent or logged: `Frame`'s `Debug` omits them, and the spike keeps only
the previous frame per camera for comparison.

Concurrency: a Tokio runtime for OBS I/O, HTTP, timers and controls. Two
`std::thread` vision workers, because MediaPipe calls are blocking and the
landmarker is `Send` but not `Sync`. A hotkey thread runs the Win32 message loop,
and a console thread reads stdin. Status is shared through a `Mutex<Status>`
that holds only numbers.

## Head pose

`face.rs` uses MediaPipe's facial transformation matrix (canonical face model
to camera space). The face's forward axis is `R·(0,0,1)`. Yaw and pitch are its
angles **relative to the direction from the face to the camera** (`-t`), so a
user who slides sideways but keeps looking at the camera stays "frontal".
Fallback when no matrix is present: the plane through the outer eye corners
(33, 263), forehead (10) and chin (152). Its normal is computed in
aspect-corrected camera space, with an off-centre correction from the nose-tip
position and an assumed 70° HFOV. Both are unit-tested on synthetic rotated
faces.

## Calibration and scores

For each camera *c*, `autocam calibrate` records the median pose it observes
while the user faces *c*'s monitor (**target**) and while facing the other
monitor (**away**, if the face is still visible). It also records the pose while
looking down.

```
score_c = exp(-½ [ (Δyaw/σ_yaw)² + (Δpitch/σ_pitch)² ])      Δ = pose − target_c
σ_yaw   = clamp(|target − away| / 3, 4°, 20°), at least 2.5 × measured noise
σ_pitch = clamp(1.25 σ_yaw, 6°, 25°)
looking_down ⇔ Δpitch < 0.6 × (down − target).pitch   (clamped to −30…−6°)
```

Because each camera's width is derived from its own target–away separation, a
camera mounted at an angle (target yaw +25°) and one mounted head-on (target
0°) produce comparable 0–1 scores. The away pose scores about 0.01 and the
half-way pose about 0.3. Calibration fails loudly when a camera cannot see the
face (< 40% of frames) or when the head barely moved (< 6° separation). It
warns below 12°, and when the predicted margin is below the configured one.

`decide()` per tick:

| Situation | Verdict |
|---|---|
| no camera sees a face | Hold(NoFace), *neutral* |
| any visible face is looking down | Hold(LookingDown), *contrary* |
| best score < `min_score` (0.45), one camera without face | Hold(TrackingLost), *neutral* |
| best score < `min_score`, both cameras see the face | Hold(LowScore), *contrary* |
| best − other < `confidence_margin` (0.25) | Hold(Ambiguous), *contrary* |
| otherwise | Prefer(best) |

A faulted camera (capture failed, black, stale) contributes no score and can
never be preferred.

## Switching state machine

`Switcher` is pure: events carry their own `Instant`, so tests use synthetic time.

- **Auto switch** to the opposing camera when it has been preferred for
  `dwell` = 800 ms with ≥ `min_votes` = 3 opposing verdicts, the deciding tick
  is itself an opposing verdict, and ≥ `min_interval` = 1500 ms have passed
  since the last switch.
- **Contrary verdicts** (current camera wins, ambiguous, low score, looking down)
  reset the dwell.
- **Neutral verdicts** (tracking lost) never switch. A run shorter than
  `max_gap` = 500 ms does not reset the dwell. `max_gap = 0` gives strict
  every-tick behaviour. This was added after simulation showed that strict mode
  lost most switches under realistic tracker dropouts (509 vs. 12 missed per
  10 h at 30% dropouts) *and* produced more false switches, because its late
  switches landed after the user had moved on (1.5/h vs. 0/h). See
  [RESULTS.md](RESULTS.md#3-switching-quality-synthetic).
- **Manual select** switches immediately (ignores the interval) and enters Manual
  mode. In Manual mode the tracker is ignored until Auto is restored, and then
  a fresh dwell is required.
- **Program changed in OBS by hand**: adopted as current, and timers restart so
  AutoCam does not fight the user.
- **Switch failed**: current camera restored. Retried after a fresh dwell.

## OBS transport and reconnect

`obws` 0.15 over obs-websocket 5. Every request has a 2 s timeout. The run loop
reconnects with exponential backoff (0.5 s, doubling to 5 s) when:

- three ticks in a row fail and a version probe fails,
- the event stream ends, or
- OBS announces `ExitStarted` / `ServerStopping`.

`CurrentProgramSceneChanged` events track manual scene changes. `check_setup`
verifies scenes and sources on every connect (keepalive presence is logged), and exposes
problems in `/status`.

## Feasibility spike

**Question:** Can `GetSourceScreenshot` sample both webcams at 4 FPS with fresh
frames, including the camera whose scene is *not* on Program?

**Method** (`autocam spike`): sample both cameras at 4 FPS for 60 s while
rotating Program between LeftCam and RightCam (and optionally an empty scene)
every 3 s. Classify every sample per camera as *own scene on Program* or *other
camera's scene on Program*. Count byte-identical consecutive frames; live sensors
never repeat exactly. Pass: ≥ 99% success, ≥ 95% fresh, p95 latency ≤ 150 ms
in both states. `--negative-control` repeats with *Deactivate when not showing*
on, which must fail, to prove the test can see staleness.

**Findings** (details in [RESULTS.md](RESULTS.md#1-feasibility-spike)):

1. **The screenshot path is fast and reliable.** With two async video sources
   (OBS Media Sources looping generated video), every sample in both states was
   fresh, with 0 failures, p95 latency 10–20 ms and 4.0 FPS achieved per camera,
   with and without keepalive. `SetCurrentProgramScene` acknowledged in
   p50 13–19 ms.
2. **Two real webcams (Dell WB7022 + Intel IPU): fresh when not shown.** The
   camera whose scene is not on Program was 97.6–100% fresh, with 0 failures and
   p95 ≤ 16 ms, **with and without** keepalive items (A/B, 60 s each).
   *Deactivate when not showing* was off.
3. **An earlier single-camera run looked frozen when not shown** (23% fresh). It
   was taken while the laptop camera was sending its privacy placeholder, and 2
   contradicts it, so it is attributed to the placeholder rather than to OBS.
4. **Keepalive.** Adding each camera to the other camera's scene as a tiny,
   locked, off-canvas item keeps both cameras `active` on Program at all times
   (verified). It is invisible in the output. On the tested webcams it is not
   needed, so it is kept as insurance against drivers that throttle hidden
   sources.

**Decision:** use the OBS path (OBS owns both cameras, GetSourceScreenshot plus
SetCurrentProgramScene, Virtual Camera to Teams). `obs-setup` also adds the
keepalive items. The runtime treats 8 identical frames in a row as a *Stale*
fault and never switches on stale data, which covers cameras that do freeze.
The native fallback below is therefore not implemented.

### Native capture fallback (not implemented)

Only needed if a future camera fails the spike even with keepalive. Design, so
that the project never opens a camera from both OBS and Rust:

- AutoCam opens both webcams with `nokhwa` (Media Foundation backend) at
  720p30 MJPEG/NV12. OBS no longer has camera inputs at all.
- AutoCam publishes each camera to OBS on loopback only. The simplest option is
  an MJPEG-over-HTTP server on `127.0.0.1` consumed by an OBS Media Source
  (`ffmpeg_source`, input `http://127.0.0.1:<port>/left.mjpg`, low-latency
  options). A lower-latency alternative is a shared-memory source plugin.
- Face tracking reads frames directly (downscaled), with no screenshot round trip.
- The `capture` module boundary (`Frame`, health checks) already separates
  acquisition from analysis. The fallback replaces `Obs::screenshot` with a
  native frame source and leaves scoring and switching untouched.

Costs: MJPEG re-encode CPU, about 1–2 frames of extra latency in the Teams
feed, and AutoCam becomes a critical part of the video path.

## Security and privacy

- No frames are persisted, transmitted or logged. Only poses, scores and
  timings are reported.
- OBS host must be loopback (validated). Password from `AUTOCAM_OBS_PASSWORD` or
  config, redacted in `Debug` and `autocam config`.
- The diagnostics HTTP server binds loopback only (validated), drops
  non-loopback peers, rejects non-loopback `Host` headers (DNS rebinding), and
  requires `X-AutoCam: 1` on POSTs (cross-site requests cannot set it without a
  CORS preflight, which is never granted). No request bodies are parsed.
- Downloads (model, libmediapipe) are SHA-256 pinned.
- obs-websocket listens on all interfaces. Mitigate with its password and the
  firewall rule in [SETUP.md](SETUP.md#2-enable-obs-websocket-with-authentication).

## Known limitations

- Head pose, not eye gaze: glancing at the other monitor with your eyes only
  will not switch, by design.
- Two cameras exactly. With *N* cameras the decision generalizes to argmax with
  a margin over the runner-up, but the config and calibration UI do not.
- Calibration assumes the user's position is roughly constant. Re-calibrate
  after moving the chair or monitors substantially.
- OBS's default 300 ms Fade transition is visible on every switch. Choose *Cut*
  for instant changes.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use autocam::config::{Config, DetectorKind};
use autocam::{app, calibrate, monitors, obs, simulate, spike};

#[derive(Parser)]
#[command(
    version,
    about = "Switch OBS between two webcam scenes based on the monitor you use (mouse) or face"
)]
struct Cli {
    /// Config file (default: %APPDATA%\AutoCam\config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Override the configured detector for this run: `mouse` or `face`.
    #[arg(long, global = true, value_parser = parse_detector)]
    detector: Option<DetectorKind>,
    /// Log filter, e.g. `info`, `debug`, `autocam=debug`.
    #[arg(long, global = true, default_value = "info")]
    log: String,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run automatic switching (default).
    Run {
        /// Do not read commands from the console.
        #[arg(long)]
        no_console: bool,
        /// Run without face tracking: manual selection only.
        #[arg(long)]
        no_face: bool,
    },
    /// Feasibility spike: verify OBS screenshots of both cameras are fresh at 4 FPS.
    Spike {
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        /// Rotate the Program scene this often (seconds).
        #[arg(long, default_value_t = 3.0)]
        rotate_every: f32,
        /// Do not rotate through an empty idle scene.
        #[arg(long)]
        no_idle: bool,
        /// Also run with "Deactivate when not showing" on, to prove staleness is detected.
        #[arg(long)]
        negative_control: bool,
        /// Temporarily disable the keepalive items (A/B test); restored afterwards.
        #[arg(long)]
        without_keepalive: bool,
        /// Write the JSON report here (no image data is included).
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Interactive calibration: face each monitor in turn.
    Calibrate {
        /// Seconds to sample per step.
        #[arg(long, default_value_t = 3.0)]
        seconds: f32,
        /// Skip the "look down at the keyboard" step.
        #[arg(long)]
        skip_down: bool,
    },
    /// Mouse: live display -> camera. Face: live head pose and scores per camera.
    Watch {
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Create one OBS scene and camera input per camera.
    ObsSetup {
        /// `NAME=DEVICE`, once per camera in order, e.g.
        /// `--camera left=dell --camera laptop=integrated --camera right=logitech`.
        /// DEVICE is part of the device name as OBS lists it.
        #[arg(long = "camera", value_parser = parse_assignment, required = true)]
        cameras: Vec<(String, String)>,
        /// OBS canvas/output size and preferred camera resolution.
        #[arg(long, default_value = "1920x1080")]
        resolution: String,
    },
    /// Simulate the decision stack on synthetic head-pose signals.
    Simulate {
        #[arg(long, default_value_t = 2.0)]
        hours: f32,
        /// Number of cameras/monitors to simulate (default: as configured).
        #[arg(long)]
        cameras: Option<usize>,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Comma-separated pose noise levels to sweep, degrees.
        #[arg(long, default_value = "2,3,5")]
        noise: String,
        #[arg(long, default_value_t = 0.02)]
        outlier_p: f32,
        #[arg(long, default_value_t = 0.05)]
        dropout_p: f32,
        /// Strict dwell: any tracking dropout restarts it (max_gap = 0).
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Re-open the camera devices in OBS (fixes cameras stuck black after a settings change).
    Reactivate,
    /// List displays, the mouse mapping, and where the cursor is.
    Monitors,
    /// Choose which displays select which camera (mouse detector).
    /// Interactive unless --assign is given.
    MapMonitors {
        /// `CAMERA=DISPLAY[,DISPLAY]`, e.g. `--assign left=DISPLAY2
        /// --assign laptop=DISPLAY1`. CAMERA is a name or number. Displays not
        /// assigned keep the current camera.
        #[arg(long = "assign", value_parser = parse_assignment)]
        assign: Vec<(String, String)>,
    },
    /// Show the config path and effective configuration (password redacted).
    Config,
}

fn parse_assignment(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.trim().is_empty() && !v.trim().is_empty() => {
            Ok((k.trim().to_owned(), v.trim().to_owned()))
        }
        _ => Err(format!("`{s}` is not NAME=VALUE")),
    }
}

fn parse_detector(s: &str) -> Result<DetectorKind, String> {
    match s.to_ascii_lowercase().as_str() {
        "mouse" => Ok(DetectorKind::Mouse),
        "face" => Ok(DetectorKind::Face),
        _ => Err(format!("unknown detector `{s}` (expected mouse or face)")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Physical pixel coordinates for the cursor and displays on mixed-DPI setups.
    autocam::mouse::enable_dpi_awareness();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(format!("{},obws=warn,tokio_websockets=warn", cli.log))
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let config_path = cli.config.clone().unwrap_or_else(Config::default_path);
    let mut cfg = Config::load(&config_path)?;
    if let Some(d) = cli.detector {
        cfg.detector = d;
    }

    match cli.cmd.unwrap_or(Cmd::Run {
        no_console: false,
        no_face: false,
    }) {
        Cmd::Run {
            no_console,
            no_face,
        } => {
            app::run(
                cfg,
                app::RunOptions {
                    config_path,
                    console: !no_console,
                    face: !no_face,
                },
            )
            .await
        }
        Cmd::Spike {
            seconds,
            rotate_every,
            no_idle,
            negative_control,
            without_keepalive,
            json,
        } => {
            let report = spike::run_spike(
                &cfg,
                &spike::SpikeOptions {
                    seconds,
                    rotate_every: Duration::from_secs_f32(rotate_every.max(0.5)),
                    include_idle: !no_idle,
                    negative_control,
                    without_keepalive,
                },
            )
            .await?;
            spike::print_report(&report);
            if let Some(path) = json {
                std::fs::write(&path, serde_json::to_string_pretty(&report)?)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
            Ok(())
        }
        Cmd::Calibrate { seconds, skip_down } => {
            calibrate::calibrate(&cfg, &config_path, seconds, !skip_down).await
        }
        Cmd::Watch { seconds } => match cfg.detector {
            DetectorKind::Mouse => monitors::watch(&cfg, seconds).await,
            DetectorKind::Face => calibrate::watch(&cfg, seconds).await,
        },
        Cmd::Monitors => monitors::list(&cfg),
        Cmd::MapMonitors { assign } => monitors::map(&config_path, &assign).await,
        Cmd::ObsSetup {
            cameras,
            resolution,
        } => {
            let res = obs::parse_resolution(&resolution)
                .with_context(|| format!("--resolution `{resolution}` is not WIDTHxHEIGHT"))?;
            let o = obs::Obs::connect(&cfg.obs).await?;
            o.setup_scenes(&mut cfg, &cameras, res).await?;
            let mut on_disk = Config::load(&config_path)?;
            let camera_count_changed = on_disk.cameras.len() != cfg.cameras.len();
            on_disk.cameras = cfg.cameras.clone();
            on_disk.save(&config_path)?;
            println!(
                "OBS scenes ready; cameras saved to {}:",
                config_path.display()
            );
            for (i, c) in cfg.cameras.iter().enumerate() {
                println!(
                    "  {} {:<10} input `{}`  scene `{}`  displays [{}]",
                    i + 1,
                    c.name,
                    c.source,
                    c.scene,
                    c.monitors.join(", ")
                );
            }
            if camera_count_changed || cfg.cameras.iter().any(|c| c.monitors.is_empty()) {
                println!("next: run `autocam map-monitors` to choose each camera's display");
            }
            Ok(())
        }
        Cmd::Simulate {
            hours,
            seed,
            noise,
            outlier_p,
            dropout_p,
            strict,
            json,
            cameras,
        } => {
            let cameras = cameras
                .unwrap_or(cfg.cameras.len())
                .clamp(2, autocam::MAX_CAMERAS);
            let decision = cfg.switching.decision();
            let mut switch = cfg.switching.params();
            if strict {
                switch.max_gap = Duration::ZERO;
            }
            let results: Vec<_> = noise
                .split(',')
                .filter_map(|n| n.trim().parse::<f32>().ok())
                .map(|noise_deg| {
                    simulate::simulate(
                        &simulate::SimParams {
                            hours,
                            seed,
                            noise_deg,
                            outlier_p,
                            dropout_p,
                            ..simulate::SimParams::for_cameras(cameras)
                        },
                        decision,
                        switch,
                    )
                })
                .collect();
            simulate::print_results(&results);
            if let Some(path) = json {
                std::fs::write(&path, serde_json::to_string_pretty(&results)?)?;
            }
            Ok(())
        }
        Cmd::Reactivate => {
            let o = obs::Obs::connect(&cfg.obs).await?;
            let inputs = o.input_names().await?;
            for s in cfg
                .cameras
                .iter()
                .map(|c| &c.source)
                .filter(|s| inputs.contains(s))
            {
                o.reactivate_camera(s).await?;
                println!("re-opened `{s}`");
            }
            Ok(())
        }
        Cmd::Config => {
            println!("config file: {}", config_path.display());
            println!(
                "password source: {}",
                if std::env::var(autocam::config::PASSWORD_ENV).is_ok() {
                    "environment"
                } else if cfg.obs.password.is_some() {
                    "config file"
                } else {
                    "none"
                }
            );
            cfg.obs.password = cfg.obs.password.map(|_| "<redacted>".into());
            print!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
    }
}

//! Builds a two-"camera" rig in OBS from looping Media Sources (async video,
//! like webcams) so the feasibility spike can run without physical cameras.
//!
//!     cargo run --release --example proxy_rig -- CONFIG_OUT LEFT.y4m RIGHT.y4m [keepalive|no-keepalive]
//!
//! Scenes: "Proxy LeftCam" / "Proxy RightCam"; inputs "Proxy Left" / "Proxy Right".
//! The OBS password is taken from the normal AutoCam config.

use anyhow::{Context, Result};
use autocam::config::Config;
use autocam::obs::Obs;
use obws::requests::inputs::Create;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("remove") {
        let cfg = Config::load(&Config::default_path())?;
        let obs = Obs::connect(&cfg.obs).await?;
        let client = obs.client();
        client
            .scenes()
            .set_current_program_scene(cfg.cameras.left.scene.as_str())
            .await
            .ok();
        for scene in [
            "Proxy LeftCam",
            "Proxy RightCam",
            autocam::spike::IDLE_SCENE,
        ] {
            client.scenes().remove(scene.into()).await.ok();
        }
        for input in ["Proxy Left", "Proxy Right"] {
            client.inputs().remove(input.into()).await.ok();
        }
        println!("proxy rig and spike idle scene removed");
        return Ok(());
    }
    let [out, left, right, rest @ ..] = args.as_slice() else {
        anyhow::bail!("usage: proxy_rig CONFIG_OUT LEFT.y4m RIGHT.y4m [keepalive|no-keepalive]");
    };
    let keepalive = rest.first().is_none_or(|k| k == "keepalive");

    let mut cfg = Config::load(&Config::default_path())?;
    cfg.cameras.left.scene = "Proxy LeftCam".into();
    cfg.cameras.left.source = "Proxy Left".into();
    cfg.cameras.right.scene = "Proxy RightCam".into();
    cfg.cameras.right.source = "Proxy Right".into();

    let obs = Obs::connect(&cfg.obs).await?;
    let client = obs.client();
    // Start from a clean rig each time.
    for scene in ["Proxy LeftCam", "Proxy RightCam"] {
        client.scenes().remove(scene.into()).await.ok();
    }
    for input in ["Proxy Left", "Proxy Right"] {
        client.inputs().remove(input.into()).await.ok();
    }
    for (scene, input, file) in [
        ("Proxy LeftCam", "Proxy Left", left),
        ("Proxy RightCam", "Proxy Right", right),
    ] {
        client.scenes().create(scene).await?;
        let path = std::fs::canonicalize(file)
            .with_context(|| format!("{file} not found"))?
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_owned();
        client
            .inputs()
            .create(Create {
                scene: scene.into(),
                input,
                kind: "ffmpeg_source",
                settings: Some(json!({
                    "is_local_file": true,
                    "local_file": path,
                    "looping": true,
                    "restart_on_activate": false,
                    "close_when_inactive": false,
                    "hw_decode": false,
                })),
                enabled: Some(true),
            })
            .await
            .with_context(|| format!("creating {input}"))?;
    }
    if keepalive {
        for a in obs.ensure_keepalive(&cfg).await? {
            println!("keepalive added: {a}");
        }
    }
    cfg.calibration = None;
    cfg.save(std::path::Path::new(out))?;
    println!("rig ready (keepalive {keepalive}); config written to {out}");
    Ok(())
}

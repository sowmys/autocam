//! User controls: console commands, global hotkeys (Windows), and the HTTP
//! endpoint (see [`crate::diagnostics::serve_http`]) all produce [`Control`]s.

use std::io::BufRead;

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::switching::Mode;
use crate::{CamId, MAX_CAMERAS};

/// A camera as the user refers to it; resolved against the config by the
/// run loop ([`crate::config::Config::find_camera`] for names and numbers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CamRef {
    Id(CamId),
    /// Name or 1-based number as typed.
    Key(String),
    /// The camera after / before the current one, wrapping around.
    Next,
    Prev,
}

impl CamRef {
    /// Parses `next`, `prev`, a 1-based number or a camera name.
    pub fn parse(s: &str) -> Option<CamRef> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "" => None,
            "next" | "n" => Some(CamRef::Next),
            "prev" | "p" | "previous" => Some(CamRef::Prev),
            _ if s
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
            {
                Some(CamRef::Key(s))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Manual selection; enters Manual mode.
    Select(CamRef),
    SetMode(Mode),
    ToggleMode,
    /// Ground-truth label for measurements: where the user is really looking
    /// (`None` = at no camera's monitor).
    Label(Option<CamRef>),
    PrintStatus,
    Quit,
}

pub const CONSOLE_HELP: &str = "commands: <number or name>=select camera  n/p=next/previous  \
     a=auto  m=manual  t=toggle  label <number|name|none>  s=status  q=quit";

pub fn parse_console(line: &str) -> Option<Control> {
    let line = line.trim().to_ascii_lowercase();
    if let Some(rest) = line.strip_prefix("label") {
        let rest = rest.trim();
        return if rest == "none" || rest == "0" {
            Some(Control::Label(None))
        } else {
            CamRef::parse(rest).map(|c| Control::Label(Some(c)))
        };
    }
    Some(match line.as_str() {
        "a" | "auto" => Control::SetMode(Mode::Auto),
        "m" | "manual" => Control::SetMode(Mode::Manual),
        "t" | "toggle" => Control::ToggleMode,
        "s" | "status" => Control::PrintStatus,
        "q" | "quit" | "exit" => Control::Quit,
        "h" | "help" | "?" => return None,
        other => Control::Select(CamRef::parse(other)?),
    })
}

/// Reads console commands on a blocking thread.
pub fn spawn_console(tx: mpsc::Sender<Control>) {
    std::thread::Builder::new()
        .name("console".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match parse_console(&line) {
                    Some(c) => {
                        if tx.blocking_send(c).is_err() {
                            break;
                        }
                    }
                    None => println!("{CONSOLE_HELP}"),
                }
            }
        })
        .expect("spawning console thread");
}

/// The global hotkeys for `cameras` cameras, as (id, modifiers, virtual key,
/// control, label). Pure, so the table is unit-tested.
///
/// - Ctrl+Alt+1..N: select that camera (manual)
/// - Ctrl+Alt+Left / Right: previous / next camera (manual)
/// - Ctrl+Alt+A: toggle auto/manual
/// - Ctrl+Alt+Shift+1..N / 0: ground-truth label camera N / none
pub fn hotkey_table(cameras: usize) -> Vec<(i32, u32, u16, Control, String)> {
    const MOD_ALT: u32 = 0x1;
    const MOD_CONTROL: u32 = 0x2;
    const MOD_SHIFT: u32 = 0x4;
    const MOD_NOREPEAT: u32 = 0x4000;
    const VK_LEFT: u16 = 0x25;
    const VK_RIGHT: u16 = 0x27;
    const VK_A: u16 = 0x41;
    let base = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
    let label = base | MOD_SHIFT;
    let digit = |d: usize| 0x30 + d as u16;

    let mut t = vec![
        (
            1,
            base,
            VK_LEFT,
            Control::Select(CamRef::Prev),
            "Ctrl+Alt+Left".into(),
        ),
        (
            2,
            base,
            VK_RIGHT,
            Control::Select(CamRef::Next),
            "Ctrl+Alt+Right".into(),
        ),
        (3, base, VK_A, Control::ToggleMode, "Ctrl+Alt+A".into()),
        (
            4,
            label,
            digit(0),
            Control::Label(None),
            "Ctrl+Alt+Shift+0".into(),
        ),
    ];
    for i in 0..cameras.min(MAX_CAMERAS) {
        let n = i + 1;
        t.push((
            10 + n as i32,
            base,
            digit(n),
            Control::Select(CamRef::Id(CamId(i))),
            format!("Ctrl+Alt+{n}"),
        ));
        t.push((
            20 + n as i32,
            label,
            digit(n),
            Control::Label(Some(CamRef::Id(CamId(i)))),
            format!("Ctrl+Alt+Shift+{n}"),
        ));
    }
    t
}

/// Registers the global hotkeys and forwards presses.
#[cfg(windows)]
pub fn spawn_hotkeys(tx: mpsc::Sender<Control>, cameras: usize) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    std::thread::Builder::new()
        .name("hotkeys".into())
        .spawn(move || {
            let keys = hotkey_table(cameras);
            let mut registered = Vec::new();
            for (id, mods, vk, _, name) in &keys {
                // SAFETY: plain Win32 call; hotkeys bind to this thread's queue.
                match unsafe { RegisterHotKey(None, *id, HOT_KEY_MODIFIERS(*mods), u32::from(*vk)) }
                {
                    Ok(()) => registered.push(name.as_str()),
                    Err(e) => {
                        warn!("could not register hotkey {name}: {e} (in use by another app?)")
                    }
                }
            }
            if !registered.is_empty() {
                info!("hotkeys: {}", registered.join(", "));
            }
            let mut msg = MSG::default();
            // SAFETY: standard message loop on the thread that owns the hotkeys.
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                if msg.message == WM_HOTKEY {
                    let id = msg.wParam.0 as i32;
                    if let Some((.., c, _)) = keys.iter().find(|k| k.0 == id)
                        && tx.blocking_send(c.clone()).is_err()
                    {
                        break;
                    }
                }
            }
            for (id, ..) in &keys {
                // SAFETY: unregisters hotkeys this thread registered.
                unsafe { UnregisterHotKey(None, *id).ok() };
            }
        })
        .expect("spawning hotkey thread");
}

#[cfg(not(windows))]
pub fn spawn_hotkeys(_tx: mpsc::Sender<Control>, _cameras: usize) {
    warn!("global hotkeys are only available on Windows");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(s: &str) -> Option<Control> {
        Some(Control::Select(CamRef::Key(s.into())))
    }

    #[test]
    fn console_commands() {
        assert_eq!(parse_console(" 2 "), sel("2"));
        assert_eq!(parse_console("Laptop"), sel("laptop"));
        assert_eq!(parse_console("n"), Some(Control::Select(CamRef::Next)));
        assert_eq!(parse_console("prev"), Some(Control::Select(CamRef::Prev)));
        assert_eq!(parse_console("a"), Some(Control::SetMode(Mode::Auto)));
        assert_eq!(parse_console("label none"), Some(Control::Label(None)));
        assert_eq!(parse_console("label 0"), Some(Control::Label(None)));
        assert_eq!(
            parse_console("label right"),
            Some(Control::Label(Some(CamRef::Key("right".into()))))
        );
        assert_eq!(parse_console("help"), None);
        assert_eq!(parse_console("two words"), None);
    }

    #[test]
    fn hotkeys_for_three_cameras() {
        let t = hotkey_table(3);
        // 4 fixed + 2 per camera, unique ids and key combinations.
        assert_eq!(t.len(), 4 + 6);
        let mut ids: Vec<i32> = t.iter().map(|k| k.0).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), t.len());
        let mut combos: Vec<(u32, u16)> = t.iter().map(|k| (k.1, k.2)).collect();
        combos.sort_unstable();
        combos.dedup();
        assert_eq!(combos.len(), t.len());
        let ctrl_alt_3 = t.iter().find(|k| k.4 == "Ctrl+Alt+3").unwrap();
        assert_eq!(ctrl_alt_3.3, Control::Select(CamRef::Id(CamId(2))));
        assert!(!t.iter().any(|k| k.4 == "Ctrl+Alt+4"));
    }
}

//! User controls: console commands, global hotkeys (Windows), and the HTTP
//! endpoint (see [`crate::diagnostics::serve_http`]) all produce [`Control`]s.

use std::io::BufRead;

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::CamId;
use crate::switching::Mode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Manual selection; enters Manual mode.
    Select(CamId),
    SetMode(Mode),
    ToggleMode,
    /// Ground-truth label for measurements: where the user is really looking.
    Label(Option<CamId>),
    PrintStatus,
    Quit,
}

pub const CONSOLE_HELP: &str = "commands: l=left  r=right  a=auto  m=manual  t=toggle  \
     1/2/0=label left/right/none  s=status  q=quit";

pub fn parse_console(line: &str) -> Option<Control> {
    Some(match line.trim().to_ascii_lowercase().as_str() {
        "l" | "left" => Control::Select(CamId::Left),
        "r" | "right" => Control::Select(CamId::Right),
        "a" | "auto" => Control::SetMode(Mode::Auto),
        "m" | "manual" => Control::SetMode(Mode::Manual),
        "t" | "toggle" => Control::ToggleMode,
        "1" => Control::Label(Some(CamId::Left)),
        "2" => Control::Label(Some(CamId::Right)),
        "0" => Control::Label(None),
        "s" | "status" => Control::PrintStatus,
        "q" | "quit" | "exit" => Control::Quit,
        _ => return None,
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

/// Registers global hotkeys and forwards presses:
/// - Ctrl+Alt+Left / Right: select camera (manual)
/// - Ctrl+Alt+A: toggle auto/manual
/// - Ctrl+Alt+Shift+Left / Right / Down: ground-truth label left / right / none
#[cfg(windows)]
pub fn spawn_hotkeys(tx: mpsc::Sender<Control>) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey,
        UnregisterHotKey, VIRTUAL_KEY, VK_A, VK_DOWN, VK_LEFT, VK_RIGHT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    std::thread::Builder::new()
        .name("hotkeys".into())
        .spawn(move || {
            let base: HOT_KEY_MODIFIERS = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
            let label = base | MOD_SHIFT;
            let keys: [(i32, HOT_KEY_MODIFIERS, VIRTUAL_KEY, Control, &str); 6] = [
                (
                    1,
                    base,
                    VK_LEFT,
                    Control::Select(CamId::Left),
                    "Ctrl+Alt+Left",
                ),
                (
                    2,
                    base,
                    VK_RIGHT,
                    Control::Select(CamId::Right),
                    "Ctrl+Alt+Right",
                ),
                (3, base, VK_A, Control::ToggleMode, "Ctrl+Alt+A"),
                (
                    4,
                    label,
                    VK_LEFT,
                    Control::Label(Some(CamId::Left)),
                    "Ctrl+Alt+Shift+Left",
                ),
                (
                    5,
                    label,
                    VK_RIGHT,
                    Control::Label(Some(CamId::Right)),
                    "Ctrl+Alt+Shift+Right",
                ),
                (
                    6,
                    label,
                    VK_DOWN,
                    Control::Label(None),
                    "Ctrl+Alt+Shift+Down",
                ),
            ];
            let mut registered = Vec::new();
            for (id, mods, vk, _, name) in &keys {
                // SAFETY: plain Win32 call; hotkeys bind to this thread's queue.
                match unsafe { RegisterHotKey(None, *id, *mods, u32::from(vk.0)) } {
                    Ok(()) => registered.push(*name),
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
                        && tx.blocking_send(*c).is_err()
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
pub fn spawn_hotkeys(_tx: mpsc::Sender<Control>) {
    warn!("global hotkeys are only available on Windows");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_commands() {
        assert_eq!(parse_console(" L "), Some(Control::Select(CamId::Left)));
        assert_eq!(parse_console("right"), Some(Control::Select(CamId::Right)));
        assert_eq!(parse_console("a"), Some(Control::SetMode(Mode::Auto)));
        assert_eq!(parse_console("0"), Some(Control::Label(None)));
        assert_eq!(parse_console("bogus"), None);
    }
}

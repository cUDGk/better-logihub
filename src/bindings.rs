use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::specialkeys::resolve_cid;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bindings {
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceBindings>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceBindings {
    #[serde(default)]
    pub gkeys: BTreeMap<String, Action>,
    #[serde(default)]
    pub cids: BTreeMap<String, Action>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Action {
    Keys(KeysAction),
    Text(TextAction),
    Run(RunAction),
    Macro(MacroAction),
    None(NoneAction),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeysAction {
    pub keys: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextAction {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunAction {
    pub run: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacroAction {
    pub r#macro: Vec<MacroStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoneAction {
    pub none: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MacroStep {
    Keys(KeysAction),
    Delay(DelayStep),
    Text(TextAction),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelayStep {
    pub delay_ms: u64,
}

pub fn default_path() -> Result<PathBuf> {
    let appdata = env::var_os("APPDATA").context("APPDATA is not set; pass --config <path>")?;
    Ok(PathBuf::from(appdata)
        .join("better-logihub")
        .join("bindings.json"))
}

pub fn load_or_create(path: &Path) -> Result<(Bindings, bool)> {
    match fs::read(path) {
        Ok(bytes) => {
            let bindings: Bindings = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            bindings.validate()?;
            Ok((bindings, false))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            let example = serde_json::json!({
                "_comment": "Replace the example key with a model_id or PID such as 0x407c; see README.md.",
                "devices": {
                    "example-model-or-pid": {
                        "gkeys": { "g1": { "keys": "ctrl+shift+c" } },
                        "cids": { "play-pause": { "keys": "media-play-pause" } }
                    }
                }
            });
            let mut bytes = serde_json::to_vec_pretty(&example)?;
            bytes.push(b'\n');
            fs::write(path, bytes)
                .with_context(|| format!("failed to create example {}", path.display()))?;
            Ok((serde_json::from_value(example)?, true))
        }
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn load(path: &Path) -> Result<Bindings> {
    match fs::read(path) {
        Ok(bytes) => {
            let bindings: Bindings = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            bindings.validate()?;
            Ok(bindings)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Bindings::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn save(path: &Path, bindings: &Bindings) -> Result<()> {
    bindings.validate()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut bytes = serde_json::to_vec_pretty(bindings)?;
    bytes.push(b'\n');
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

impl Bindings {
    pub fn validate(&self) -> Result<()> {
        for (device, bindings) in &self.devices {
            ensure!(
                !device.trim().is_empty(),
                "device selector must not be empty"
            );
            for (gkey, action) in &bindings.gkeys {
                parse_gkey(gkey)?;
                action.validate()?;
            }
            for (cid, action) in &bindings.cids {
                resolve_cid(cid)?;
                action.validate()?;
            }
        }
        Ok(())
    }
}

impl Action {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Keys(action) => {
                parse_key_chord(&action.keys)?;
            }
            Self::Text(_) => {}
            Self::Run(action) => ensure!(!action.run.trim().is_empty(), "run action is empty"),
            Self::Macro(action) => {
                ensure!(
                    !action.r#macro.is_empty(),
                    "macro must contain at least one step"
                );
                for step in &action.r#macro {
                    if let MacroStep::Keys(action) = step {
                        parse_key_chord(&action.keys)?;
                    }
                }
            }
            Self::None(action) => ensure!(action.none, "none must be true"),
        }
        Ok(())
    }

    pub fn execute(&self) -> Result<()> {
        match self {
            Self::Keys(action) => send_chord(&action.keys),
            Self::Text(action) => send_text(&action.text),
            Self::Run(action) => {
                Command::new("cmd.exe")
                    .args(["/D", "/S", "/C", &action.run])
                    .spawn()
                    .with_context(|| format!("failed to run {:?}", action.run))?;
                Ok(())
            }
            Self::Macro(action) => {
                for step in &action.r#macro {
                    match step {
                        MacroStep::Keys(action) => send_chord(&action.keys)?,
                        MacroStep::Delay(step) => {
                            thread::sleep(Duration::from_millis(step.delay_ms))
                        }
                        MacroStep::Text(action) => send_text(&action.text)?,
                    }
                }
                Ok(())
            }
            Self::None(_) => Ok(()),
        }
    }

    pub fn description(&self) -> String {
        match self {
            Self::Keys(action) => format!("keys:{}", action.keys),
            Self::Text(action) => format!("text:{} chars", action.text.chars().count()),
            Self::Run(action) => format!("run:{}", action.run),
            Self::Macro(action) => format!("macro:{} steps", action.r#macro.len()),
            Self::None(_) => "none".into(),
        }
    }
}

pub fn parse_gkey(value: &str) -> Result<u8> {
    let value = value.trim().to_ascii_lowercase();
    let number = value
        .strip_prefix('g')
        .and_then(|value| value.parse::<u8>().ok())
        .context("G-key name must be g1..g32")?;
    ensure!((1..=32).contains(&number), "G-key name must be g1..g32");
    Ok(number)
}

fn parse_key_chord(value: &str) -> Result<Vec<u16>> {
    let mut keys = Vec::new();
    for name in value.split('+') {
        let name = name.trim();
        ensure!(!name.is_empty(), "empty key in chord {value:?}");
        let vk = vk_for_name(name).with_context(|| format!("unknown key name {name:?}"))?;
        ensure!(!keys.contains(&vk), "duplicate key {name:?}");
        keys.push(vk);
    }
    ensure!(!keys.is_empty(), "key chord is empty");
    Ok(keys)
}

pub fn vk_for_name(value: &str) -> Option<u16> {
    let name = value.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    if name.len() == 1 {
        return match name.as_bytes()[0] {
            b'a'..=b'z' => Some(u16::from(name.as_bytes()[0].to_ascii_uppercase())),
            b'0'..=b'9' => Some(u16::from(name.as_bytes()[0])),
            _ => None,
        };
    }
    if let Some(number) = name
        .strip_prefix('f')
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| (1..=24).contains(value))
    {
        return Some(0x70 + number - 1);
    }
    Some(match name.as_str() {
        "ctrl" | "control" => 0x11,
        "shift" => 0x10,
        "alt" => 0x12,
        "win" | "windows" | "meta" | "super" => 0x5B,
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "tab" => 0x09,
        "space" => 0x20,
        "minus" => 0xBD,
        "equal" => 0xBB,
        "leftbracket" | "left-bracket" => 0xDB,
        "rightbracket" | "right-bracket" => 0xDD,
        "backslash" => 0xDC,
        "semicolon" => 0xBA,
        "quote" => 0xDE,
        "grave" | "backtick" => 0xC0,
        "comma" => 0xBC,
        "period" | "dot" => 0xBE,
        "slash" => 0xBF,
        "capslock" | "caps-lock" => 0x14,
        "printscreen" | "print-screen" => 0x2C,
        "scrolllock" | "scroll-lock" => 0x91,
        "pause" => 0x13,
        "insert" => 0x2D,
        "home" => 0x24,
        "pageup" | "page-up" | "pgup" => 0x21,
        "delete" | "del" => 0x2E,
        "end" => 0x23,
        "pagedown" | "page-down" | "pgdn" => 0x22,
        "left" | "left-arrow" => 0x25,
        "up" | "up-arrow" => 0x26,
        "right" | "right-arrow" => 0x27,
        "down" | "down-arrow" => 0x28,
        "media-next" | "next-track" => 0xB0,
        "media-prev" | "media-previous" | "previous-track" => 0xB1,
        "media-stop" => 0xB2,
        "media-play-pause" | "play-pause" => 0xB3,
        "volume-mute" | "media-mute" | "mute" => 0xAD,
        "volume-down" => 0xAE,
        "volume-up" => 0xAF,
        _ => return None,
    })
}

fn is_extended_vk(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x2E | 0x5B..=0x5C | 0xAD..=0xB3
    )
}

#[cfg(windows)]
fn send_chord(value: &str) -> Result<()> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    };

    let keys = parse_key_chord(value)?;
    let mut inputs = Vec::with_capacity(keys.len() * 2);
    for vk in &keys {
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: *vk,
                    dwFlags: if is_extended_vk(*vk) {
                        KEYEVENTF_EXTENDEDKEY
                    } else {
                        0
                    },
                    ..Default::default()
                },
            },
        });
    }
    for vk in keys.iter().rev() {
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: *vk,
                    dwFlags: KEYEVENTF_KEYUP
                        | if is_extended_vk(*vk) {
                            KEYEVENTF_EXTENDEDKEY
                        } else {
                            0
                        },
                    ..Default::default()
                },
            },
        });
    }
    send_inputs(&inputs)
}

#[cfg(not(windows))]
fn send_chord(_: &str) -> Result<()> {
    anyhow::bail!("SendInput actions are supported only on Windows")
}

#[cfg(windows)]
fn send_text(value: &str) -> Result<()> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    };

    let mut inputs = Vec::new();
    for unit in value.encode_utf16() {
        for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wScan: unit,
                        dwFlags: flags,
                        ..Default::default()
                    },
                },
            });
        }
    }
    send_inputs(&inputs)
}

#[cfg(not(windows))]
fn send_text(_: &str) -> Result<()> {
    anyhow::bail!("SendInput actions are supported only on Windows")
}

#[cfg(windows)]
fn send_inputs(inputs: &[windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT]) -> Result<()> {
    use std::mem::size_of;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{INPUT, SendInput};

    if inputs.is_empty() {
        return Ok(());
    }
    let count = u32::try_from(inputs.len()).context("too many SendInput events")?;
    let sent = unsafe { SendInput(count, inputs.as_ptr(), size_of::<INPUT>() as i32) };
    ensure!(
        sent == count,
        "SendInput accepted {sent} of {count} events: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_bindings_shape() {
        let json = r#"{
          "devices": {
            "g915": {
              "gkeys": {
                "g1": {"keys":"ctrl+shift+c"},
                "g2": {"macro":[{"keys":"alt+tab"},{"delay_ms":50},{"text":"hello"}]},
                "g3": {"run":"C:\\\\Tools\\\\app.exe --flag"},
                "g4": {"none":true}
              },
              "cids": {"play-pause":{"keys":"media-play-pause"}}
            }
          }
        }"#;
        let bindings: Bindings = serde_json::from_str(json).unwrap();
        bindings.validate().unwrap();
        assert_eq!(bindings.devices["g915"].gkeys.len(), 4);
    }

    #[test]
    fn maps_required_key_names_to_virtual_keys() {
        assert_eq!(vk_for_name("ctrl"), Some(0x11));
        assert_eq!(vk_for_name("A"), Some(0x41));
        assert_eq!(vk_for_name("0"), Some(0x30));
        assert_eq!(vk_for_name("f24"), Some(0x87));
        assert_eq!(vk_for_name("enter"), Some(0x0D));
        assert_eq!(vk_for_name("left-arrow"), Some(0x25));
        assert_eq!(vk_for_name("media-play-pause"), Some(0xB3));
        assert_eq!(vk_for_name("volume-up"), Some(0xAF));
        assert!(vk_for_name("hyper").is_none());
    }

    #[test]
    fn rejects_ambiguous_or_invalid_actions() {
        assert!(serde_json::from_str::<Action>(r#"{"keys":"a","text":"b"}"#).is_err());
        let bindings: Bindings = serde_json::from_str(
            r#"{"devices":{"g915":{"gkeys":{"g0":{"none":true}},"cids":{}}}}"#,
        )
        .unwrap();
        assert!(bindings.validate().is_err());
    }
}

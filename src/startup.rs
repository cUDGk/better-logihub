use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::discovery::{Discovery, ManagedDevice};
use crate::gkeys::GKeys;
use crate::lighting::rgb::RgbColor;
use crate::live::{self, RgbSetting};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Startup {
    #[serde(default)]
    pub devices: BTreeMap<String, StartupDevice>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupDevice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rgb: Vec<RgbSetting>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub perkey: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perkey_fill: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gkeys_software_mode: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartupApplyResult {
    pub device: usize,
    pub name: String,
    pub selector: String,
    pub applied: Vec<String>,
}

pub fn default_path() -> Result<PathBuf> {
    let appdata = env::var_os("APPDATA").context("APPDATA is not set")?;
    Ok(PathBuf::from(appdata)
        .join("better-logihub")
        .join("startup.json"))
}

pub fn load(path: &Path) -> Result<Startup> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let startup: Startup = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    startup.validate()?;
    Ok(startup)
}

pub fn load_optional(path: &Path) -> Result<Option<Startup>> {
    match fs::read(path) {
        Ok(bytes) => {
            let startup: Startup = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            startup.validate()?;
            Ok(Some(startup))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn init(path: &Path) -> Result<bool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let example = serde_json::json!({
        "devices": {
            "g915": {
                "brightness": 20,
                "rgb": [{
                    "zone": "all",
                    "effect": "fixed",
                    "color": "004080",
                    "persist": "ram"
                }],
                "perkey_fill": "001020",
                "perkey": {"esc": "FF2000"},
                "gkeys_software_mode": false
            }
        }
    });
    let mut bytes = serde_json::to_vec_pretty(&example)?;
    bytes.push(b'\n');
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(&bytes)
                .with_context(|| format!("failed to write {}", path.display()))?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to create {}", path.display())),
    }
}

impl Startup {
    pub fn validate(&self) -> Result<()> {
        for (selector, device) in &self.devices {
            ensure!(
                !selector.trim().is_empty(),
                "device selector must not be empty"
            );
            if let Some(brightness) = device.brightness {
                ensure!(brightness <= 100, "brightness must be 0..=100");
            }
            for rgb in &device.rgb {
                rgb.validate()?;
            }
            if let Some(color) = &device.perkey_fill {
                color.parse::<RgbColor>()?;
            }
            for color in device.perkey.values() {
                color.parse::<RgbColor>()?;
            }
        }
        Ok(())
    }

    pub fn matching<'a>(&'a self, target: &ManagedDevice) -> Option<(&'a str, &'a StartupDevice)> {
        self.devices.iter().find_map(|(selector, settings)| {
            selector_matches(selector, target).then_some((selector.as_str(), settings))
        })
    }
}

pub fn selector_matches(selector: &str, target: &ManagedDevice) -> bool {
    let model_match = target
        .model
        .is_some_and(|model| model.model_id.eq_ignore_ascii_case(selector));
    let pid_match = target
        .pid
        .is_some_and(|pid| parse_pid(selector) == Some(pid));
    model_match || pid_match
}

pub fn apply(
    discovery: &Discovery,
    index: Option<usize>,
    startup: &Startup,
) -> Result<Vec<StartupApplyResult>> {
    let targets = match index {
        Some(index) => vec![
            discovery
                .devices
                .iter()
                .find(|device| device.index == index)
                .with_context(|| format!("device index {index} was not found or is a receiver"))?,
        ],
        None => discovery.devices.iter().collect(),
    };
    let mut results = Vec::new();
    for target in targets {
        if let Some((selector, settings)) = startup.matching(target) {
            results.push(StartupApplyResult {
                device: target.index,
                name: target.name.clone(),
                selector: selector.into(),
                applied: apply_device(target, settings, true)?,
            });
        }
    }
    ensure!(
        !results.is_empty(),
        "no selected device matches a selector in startup.json"
    );
    Ok(results)
}

pub fn apply_device(
    target: &ManagedDevice,
    settings: &StartupDevice,
    apply_gkeys: bool,
) -> Result<Vec<String>> {
    let mut applied = Vec::new();
    if let Some(brightness) = settings.brightness {
        let effective = live::set_brightness_percent(target, brightness)?;
        applied.push(format!("brightness {}%", effective.percent));
    }
    for rgb in &settings.rgb {
        let zones = live::apply_rgb_setting(target, rgb)?;
        applied.push(format!("rgb {} on {:?}", rgb.effect, zones));
    }
    if let Some(color) = &settings.perkey_fill {
        let result = live::apply_perkey_fill(target, None, color, false)?;
        applied.push(format!(
            "perkey_fill {} ({} zones)",
            color, result.zone_count
        ));
    }
    if !settings.perkey.is_empty() {
        let result = live::apply_perkey_map(target, &settings.perkey, false)?;
        applied.push(format!("perkey {} keys", result.zone_count));
    }
    if apply_gkeys && let Some(enabled) = settings.gkeys_software_mode {
        GKeys::new(&target.device)?.enable_software_control(enabled)?;
        applied.push(format!(
            "gkeys_software_mode {}",
            if enabled { "on" } else { "off" }
        ));
    }
    Ok(applied)
}

fn parse_pid(value: &str) -> Option<u16> {
    let value = value.trim();
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))?;
    u16::from_str_radix(value, 16).ok()
}

pub fn require_nonempty(startup: &Startup) -> Result<()> {
    if startup.devices.is_empty() {
        bail!("startup.json has no device entries");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::RgbZone;

    #[test]
    fn parses_complete_startup_schema() {
        let value = r#"{
          "devices": {
            "g915": {
              "brightness": 42,
              "rgb": [{
                "zone": "all", "effect": "breathing", "color": "FF0000",
                "color2": "0000FF", "period_ms": 3000, "speed": 20,
                "brightness": 80, "direction": "horizontal", "persist": "ram"
              }, {"zone": 1, "effect": "off"}],
              "perkey": {"a":"112233"},
              "perkey_fill": "000000",
              "gkeys_software_mode": true
            }
          }
        }"#;
        let startup: Startup = serde_json::from_str(value).unwrap();
        startup.validate().unwrap();
        assert_eq!(startup.devices["g915"].brightness, Some(42));
        assert_eq!(
            startup.devices["g915"].rgb[0].zone,
            RgbZone::Name("all".into())
        );
        assert_eq!(startup.devices["g915"].rgb[1].zone, RgbZone::Index(1));
    }

    #[test]
    fn rejects_unsafe_or_malformed_values() {
        let bad_brightness: Startup =
            serde_json::from_str(r#"{"devices":{"g915":{"brightness":101}}}"#).unwrap();
        assert!(bad_brightness.validate().is_err());
        assert!(
            serde_json::from_str::<Startup>(
                r#"{"devices":{"g915":{"rgb":[{"zone":0,"effect":"fixed","persist":"disk"}]}}}"#,
            )
            .is_err()
        );
    }
}

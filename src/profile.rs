use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::bindings::Action;
use crate::lighting::rgb::{Effect, EffectOptions, RgbColor, encode_effect, parse_direction};
use crate::onboard::{
    Binding as OnboardBinding, DpiTable, ExportBinding, LedSlot, Macro as OnboardMacro,
    OnboardExport, parse_binding, repack_export_macros,
};

const STORE_VERSION: u32 = 1;
const PORTABLE_ONBOARD_VERSION: u32 = 1;
const RGB_PRESET_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub application_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_models: Vec<String>,
    pub dpi_levels: Vec<u16>,
    #[serde(default)]
    pub default_dpi: u16,
    pub active_dpi: u16,
    pub shift_dpi: u16,
    pub report_rate_hz: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<ProfileBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macros: Vec<ImportedMacro>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lighting: Vec<RgbPreset>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileBinding {
    pub slot_id: String,
    pub device_model: String,
    pub slot_prefix: String,
    pub input: String,
    pub mode: u8,
    pub shifted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribute: Option<String>,
    pub card_id: String,
    pub source_action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_action: Option<Action>,
    pub onboard_binding: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboard_macro: Option<OnboardMacro>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedMacro {
    pub card_id: String,
    pub name: String,
    pub macro_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_action: Option<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboard_macro: Option<OnboardMacro>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgbPreset {
    pub zone: String,
    pub effect: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color2: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intensity: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    pub persist: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileStore {
    pub version: u32,
    pub profiles: Vec<Profile>,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            profiles: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableOnboardProfile {
    pub version: u32,
    pub kind: String,
    pub device_model: String,
    pub profile: Profile,
}

impl PortableOnboardProfile {
    pub fn new(device_model: String, mut profile: Profile) -> Self {
        profile.device_models = vec![device_model.clone()];
        Self {
            version: PORTABLE_ONBOARD_VERSION,
            kind: "better-logihub-onboard-profile".into(),
            device_model,
            profile,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgbPresetFile {
    pub version: u32,
    pub profile: String,
    pub device_model: String,
    pub command: String,
    pub invocations: Vec<RgbPreset>,
}

impl RgbPresetFile {
    pub fn new(profile: &Profile, device_model: &str) -> Self {
        Self {
            version: RGB_PRESET_VERSION,
            profile: profile.name.clone(),
            device_model: device_model.into(),
            command: "logihub rgb set".into(),
            invocations: profile.lighting.clone(),
        }
    }
}

pub fn default_store_path() -> Result<PathBuf> {
    let base = env::var_os("APPDATA").context("APPDATA is not set")?;
    Ok(PathBuf::from(base)
        .join("better-logihub")
        .join("profiles.json"))
}

pub fn default_output_dir() -> Result<PathBuf> {
    default_store_path()?
        .parent()
        .map(Path::to_path_buf)
        .context("profile store has no parent directory")
}

pub fn load_store(path: &Path) -> Result<ProfileStore> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProfileStore::default());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read profile store {}", path.display()));
        }
    };
    let store: ProfileStore = serde_json::from_slice(&contents)
        .with_context(|| format!("failed to parse profile store {}", path.display()))?;
    if store.version != STORE_VERSION {
        bail!(
            "unsupported profile store version {} in {} (expected {STORE_VERSION})",
            store.version,
            path.display()
        );
    }
    Ok(store)
}

pub fn save_store(path: &Path, store: &ProfileStore) -> Result<()> {
    save_json(path, store, "profiles")
}

pub fn save_json<T: Serialize + ?Sized>(path: &Path, value: &T, label: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create profile directory {}", parent.display()))?;
    }
    let mut json =
        serde_json::to_vec_pretty(value).with_context(|| format!("failed to serialize {label}"))?;
    json.push(b'\n');
    fs::write(path, json).with_context(|| format!("failed to write {label} {}", path.display()))
}

pub fn load_portable_onboard(path: &Path) -> Result<Option<PortableOnboardProfile>> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if value.get("kind").and_then(serde_json::Value::as_str)
        != Some("better-logihub-onboard-profile")
    {
        return Ok(None);
    }
    let portable: PortableOnboardProfile = serde_json::from_value(value)?;
    ensure!(
        portable.version == PORTABLE_ONBOARD_VERSION,
        "unsupported portable onboard profile version {}",
        portable.version
    );
    Ok(Some(portable))
}

pub fn merge_profiles(store: &mut ProfileStore, imported: &[Profile]) {
    for profile in imported {
        if let Some(existing) = store
            .profiles
            .iter_mut()
            .find(|existing| existing.name == profile.name)
        {
            *existing = profile.clone();
        } else {
            store.profiles.push(profile.clone());
        }
    }
}

pub fn apply_to_onboard_export(
    profile: &Profile,
    device_model: &str,
    export: &mut OnboardExport,
) -> Result<Vec<String>> {
    ensure!(
        profile
            .device_models
            .iter()
            .any(|model| model.eq_ignore_ascii_case(device_model)),
        "profile {:?} has no settings for device model {device_model}",
        profile.name
    );
    let keyboard = export.device_type.eq_ignore_ascii_case("KEYBOARD");
    let mut warnings = Vec::new();
    let relevant = profile
        .bindings
        .iter()
        .filter(|binding| binding.device_model.eq_ignore_ascii_case(device_model))
        .collect::<Vec<_>>();
    let mut modes = relevant
        .iter()
        .map(|binding| binding.mode)
        .collect::<Vec<_>>();
    if modes.is_empty() {
        modes.push(1);
    }
    modes.sort_unstable();
    modes.dedup();

    for mode in modes {
        let Some(target) = export.profiles.get_mut(usize::from(mode.saturating_sub(1))) else {
            warnings.push(format!(
                "M{mode} assignments exceed the device's {} onboard profile slots",
                export.profiles.len()
            ));
            continue;
        };
        target.name = Some(onboard_name(&profile.name));
        if profile.report_rate_hz != 0 {
            target.rate_hz = Some(profile.report_rate_hz);
        }
        if !keyboard && !profile.dpi_levels.is_empty() {
            target.dpi = Some(dpi_table(profile)?);
        }
        for binding in &mut target.bindings {
            *binding = unassigned_export_binding(binding.number, &binding.control)?;
        }
        for binding in &mut target.gshift_bindings {
            *binding = unassigned_export_binding(binding.number, &binding.control)?;
        }

        let mut inherited = Vec::new();
        for binding in relevant.iter().filter(|binding| binding.mode == mode) {
            let Some(number) = binding
                .input
                .strip_prefix('g')
                .and_then(|value| value.parse::<usize>().ok())
            else {
                warnings.push(format!(
                    "{} is software-only and has no onboard button-table entry",
                    binding.slot_id
                ));
                continue;
            };
            let bank = if binding.shifted {
                &mut target.gshift_bindings
            } else {
                &mut target.bindings
            };
            if !(1..=bank.len()).contains(&number) {
                warnings.push(format!(
                    "{} selects button {number}, but the target has {} entries",
                    binding.slot_id,
                    bank.len()
                ));
                continue;
            }
            if binding.onboard_binding == "inherit" {
                inherited.push(number);
                continue;
            }
            bank[number - 1] = imported_export_binding(
                number,
                &bank[number - 1].control,
                &binding.onboard_binding,
                binding.onboard_macro.clone(),
            )?;
            warnings.extend(binding.warnings.iter().cloned());
        }
        for number in inherited {
            target.gshift_bindings[number - 1] = target.bindings[number - 1].clone();
        }

        for (slot, preset) in profile.lighting.iter().take(4).enumerate() {
            if slot >= target.led_slots.len() {
                warnings.push(format!(
                    "lighting slot {slot} exceeds the device's {} LED slots",
                    target.led_slots.len()
                ));
                continue;
            }
            match led_slot(slot, preset) {
                Ok(value) => target.led_slots[slot] = value,
                Err(error) => {
                    warnings.push(format!(
                        "{} cannot be stored in onboard LED slot {slot}: {error}; replaced with OFF",
                        preset.effect
                    ));
                    target.led_slots[slot] = off_led_slot(slot);
                }
            }
        }
        if profile.lighting.len() > 4 {
            warnings.push(format!(
                "{} lighting effects exceed the four layout-A LED slots",
                profile.lighting.len()
            ));
        }
    }
    repack_export_macros(export)?;
    warnings.sort();
    warnings.dedup();
    Ok(warnings)
}

fn dpi_table(profile: &Profile) -> Result<DpiTable> {
    let default = if profile.default_dpi == 0 {
        profile.active_dpi
    } else {
        profile.default_dpi
    };
    let default_index = profile
        .dpi_levels
        .iter()
        .position(|value| *value == default)
        .with_context(|| format!("default DPI {default} is absent from the DPI table"))?;
    let shift_index = if profile.shift_dpi == 0 {
        None
    } else {
        Some(
            profile
                .dpi_levels
                .iter()
                .position(|value| *value == profile.shift_dpi)
                .with_context(|| {
                    format!(
                        "shift DPI {} is absent from the DPI table",
                        profile.shift_dpi
                    )
                })? as u8,
        )
    };
    Ok(DpiTable {
        levels: profile.dpi_levels.clone(),
        default_index: default_index as u8,
        shift_index,
    })
}

fn unassigned_export_binding(number: usize, control: &str) -> Result<ExportBinding> {
    imported_export_binding(number, control, "disabled", None)
}

fn imported_export_binding(
    number: usize,
    control: &str,
    binding: &str,
    r#macro: Option<OnboardMacro>,
) -> Result<ExportBinding> {
    let parsed = if r#macro.is_some() {
        OnboardBinding::Macro {
            sector: 0,
            offset: 0,
        }
    } else {
        parse_binding(binding)?
    };
    let raw = parsed.encode()?;
    Ok(ExportBinding {
        number,
        control: control.into(),
        binding: parsed.to_string(),
        raw_hex: hex(&raw),
        r#macro,
    })
}

fn led_slot(slot: usize, preset: &RgbPreset) -> Result<LedSlot> {
    let effect = Effect::parse(&preset.effect)?;
    ensure!(
        effect.name != "streaming",
        "STREAMING is live-only according to firmware_lighting"
    );
    let options = EffectOptions {
        color: preset
            .color
            .as_deref()
            .map(str::parse::<RgbColor>)
            .transpose()?,
        color2: preset
            .color2
            .as_deref()
            .map(str::parse::<RgbColor>)
            .transpose()?,
        speed: preset.speed,
        period_ms: preset.period,
        brightness: preset.brightness,
        intensity: preset.intensity,
        direction: preset
            .direction
            .as_deref()
            .map(parse_direction)
            .transpose()?,
    };
    let raw_id = u8::try_from(effect.raw_id).context("RGB effect id does not fit an LED slot")?;
    let parameters = encode_effect(effect, &options)?;
    let mut raw = Vec::with_capacity(11);
    raw.push(raw_id);
    raw.extend_from_slice(&parameters);
    Ok(LedSlot {
        slot,
        effect: effect.name.into(),
        raw_id,
        parameters_hex: hex(&parameters),
        raw_hex: hex(&raw),
    })
}

fn off_led_slot(slot: usize) -> LedSlot {
    LedSlot {
        slot,
        effect: "off".into(),
        raw_id: 0,
        parameters_hex: "00000000000000000000".into(),
        raw_hex: "0000000000000000000000".into(),
    }
}

fn onboard_name(value: &str) -> String {
    let mut result = String::new();
    let mut units = 0;
    for character in value.chars() {
        let count = character.len_utf16();
        if units + count > 24 {
            break;
        }
        result.push(character);
        units += count;
    }
    result
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::{Description, DirectoryEntry, ExportDirectory, ExportProfile, RawSector};

    #[test]
    fn merge_overwrites_same_name_and_keeps_other_profiles() {
        let mut store = ProfileStore {
            version: 1,
            profiles: vec![profile("Desktop", 800), profile("Existing Game", 1600)],
        };
        let imported = vec![profile("Desktop", 4000), profile("New Game", 3200)];

        merge_profiles(&mut store, &imported);

        assert_eq!(store.profiles.len(), 3);
        assert_eq!(store.profiles[0], profile("Desktop", 4000));
        assert_eq!(store.profiles[1], profile("Existing Game", 1600));
        assert_eq!(store.profiles[2], profile("New Game", 3200));
    }

    #[test]
    fn older_version_one_profiles_gain_empty_phase_e_fields() {
        let value = r#"{
          "version":1,
          "profiles":[{
            "name":"Desktop","source":"ghub-import","dpi_levels":[800],
            "active_dpi":800,"shift_dpi":800,"report_rate_hz":1000
          }]
        }"#;
        let store: ProfileStore = serde_json::from_str(value).unwrap();
        assert!(store.profiles[0].bindings.is_empty());
        assert_eq!(store.profiles[0].default_dpi, 0);
    }

    #[test]
    fn imported_profile_overlays_the_phase_d_export_schema() {
        let imported = crate::ghub_import::import_ghub_json(
            include_str!("../tests/fixtures/ghub_minimal.json"),
            None,
        )
        .unwrap();
        let mut export = synthetic_mouse_export();

        let warnings =
            apply_to_onboard_export(&imported.profiles[0], "g502x_lightspeed", &mut export)
                .unwrap();

        assert!(warnings.is_empty(), "{warnings:?}");
        let profile = &export.profiles[0];
        assert_eq!(profile.dpi.as_ref().unwrap().default_index, 3);
        assert_eq!(profile.dpi.as_ref().unwrap().shift_index, Some(0));
        assert_eq!(profile.rate_hz, Some(1000));
        assert_eq!(profile.bindings[4].binding, "key:f5");
        assert_eq!(profile.gshift_bindings[4].binding, "key:f5");
        assert!(profile.bindings[6].binding.starts_with("macro:"));
        assert_eq!(export.macro_sectors.len(), 2);
        assert_eq!(profile.led_slots[0].effect, "breathing");
    }

    fn synthetic_mouse_export() -> OnboardExport {
        let binding = |number| ExportBinding {
            number,
            control: format!("g{number}"),
            binding: "disabled".into(),
            raw_hex: "FFFFFFFF".into(),
            r#macro: None,
        };
        let bindings = (1..=11).map(binding).collect::<Vec<_>>();
        let led_slot = |slot| LedSlot {
            slot,
            effect: "off".into(),
            raw_id: 0,
            parameters_hex: "00000000000000000000".into(),
            raw_hex: "0000000000000000000000".into(),
        };
        OnboardExport {
            version: 1,
            device_type: "MOUSE".into(),
            description: Description {
                raw: [0; 16],
                memory_model_id: 1,
                profile_format_id: 1,
                macro_format_id: 1,
                profile_count: 1,
                profile_count_oob: 0,
                button_count: 11,
                sector_count: 4,
                sector_size: 255,
                mechanical_layout: 0,
                various_info: 0,
            },
            directory: ExportDirectory {
                entries: vec![DirectoryEntry {
                    index: 0,
                    sector: 1,
                    enabled: true,
                }],
                raw_sector_hex: String::new(),
            },
            profiles: vec![ExportProfile {
                index: 0,
                sector: 1,
                enabled: true,
                name: None,
                rate_hz: None,
                dpi: None,
                bindings: bindings.clone(),
                gshift_bindings: bindings,
                led_slots: (0..4).map(led_slot).collect(),
                raw_sector_hex: String::new(),
            }],
            macro_sectors: Vec::<RawSector>::new(),
        }
    }

    fn profile(name: &str, active_dpi: u16) -> Profile {
        Profile {
            name: name.into(),
            source: "ghub-import".into(),
            id: String::new(),
            application_id: String::new(),
            device_models: Vec::new(),
            dpi_levels: vec![active_dpi],
            default_dpi: active_dpi,
            active_dpi,
            shift_dpi: active_dpi,
            report_rate_hz: 1000,
            bindings: Vec::new(),
            macros: Vec::new(),
            lighting: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::discovery::ManagedDevice;
use crate::lighting::brightness::{Brightness, BrightnessInfo, percent_from_raw, raw_from_percent};
use crate::lighting::perkey::{
    PerKeyLightingV2, ResolvedKey, ZoneScheme, resolve_key, zones_from_usages,
};
use crate::lighting::rgb::{
    Effect, EffectOptions, Persistence, RgbCapabilities, RgbColor, RgbEffects, encode_effect,
    parse_direction,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RgbZone {
    Index(u8),
    Name(String),
}

impl fmt::Display for RgbZone {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Index(index) => write!(formatter, "{index}"),
            Self::Name(name) => formatter.write_str(name),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RgbPersistence {
    #[default]
    Ram,
    Nvm,
    Powersave,
}

impl From<RgbPersistence> for Persistence {
    fn from(value: RgbPersistence) -> Self {
        match value {
            RgbPersistence::Ram => Self::Ram,
            RgbPersistence::Nvm => Self::Nvm,
            RgbPersistence::Powersave => Self::PowerSave,
        }
    }
}

impl From<Persistence> for RgbPersistence {
    fn from(value: Persistence) -> Self {
        match value {
            Persistence::Ram => Self::Ram,
            Persistence::Nvm => Self::Nvm,
            Persistence::PowerSave => Self::Powersave,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RgbSetting {
    pub zone: RgbZone,
    pub effect: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color2: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_ms: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intensity: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    #[serde(default)]
    pub persist: RgbPersistence,
}

impl RgbSetting {
    pub fn validate(&self) -> Result<()> {
        Effect::parse(&self.effect)?;
        self.color
            .as_deref()
            .map(str::parse::<RgbColor>)
            .transpose()?;
        self.color2
            .as_deref()
            .map(str::parse::<RgbColor>)
            .transpose()?;
        if let Some(direction) = &self.direction {
            parse_direction(direction)?;
        }
        if let Some(brightness) = self.brightness {
            ensure!(brightness <= 100, "brightness must be 0..=100");
        }
        if let Some(intensity) = self.intensity {
            ensure!(intensity <= 100, "intensity must be 0..=100");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct BrightnessApplied {
    pub info: BrightnessInfo,
    pub requested_raw: u16,
    pub effective_raw: u16,
    pub percent: u8,
}

pub fn set_brightness_percent(target: &ManagedDevice, percent: u8) -> Result<BrightnessApplied> {
    let brightness = Brightness::new(&target.device)?;
    let info = brightness.info()?;
    let requested_raw = raw_from_percent(info, percent)?;
    let effective_raw = brightness.set_brightness(requested_raw)?;
    Ok(BrightnessApplied {
        info,
        requested_raw,
        effective_raw,
        percent: percent_from_raw(info, effective_raw),
    })
}

pub fn apply_rgb_setting(target: &ManagedDevice, setting: &RgbSetting) -> Result<Vec<u8>> {
    setting.validate()?;
    let effect = Effect::parse(&setting.effect)?;
    let options = EffectOptions {
        color: setting.color.as_deref().map(str::parse).transpose()?,
        color2: setting.color2.as_deref().map(str::parse).transpose()?,
        speed: setting.speed,
        period_ms: setting.period_ms,
        brightness: setting.brightness,
        intensity: setting.intensity,
        direction: setting
            .direction
            .as_deref()
            .map(parse_direction)
            .transpose()?,
    };
    apply_rgb_effect(
        target,
        &setting.zone.to_string(),
        effect,
        &options,
        setting.persist.into(),
    )
}

pub fn apply_rgb_effect(
    target: &ManagedDevice,
    zone: &str,
    effect: Effect,
    options: &EffectOptions,
    persistence: Persistence,
) -> Result<Vec<u8>> {
    let rgb = RgbEffects::new(&target.device)?;
    let capabilities = rgb.capabilities()?;
    let zones = select_rgb_zones(&capabilities, zone)?;
    let params = encode_effect(effect, options)?;
    for cluster_index in &zones {
        let cluster = &capabilities.clusters[usize::from(*cluster_index)];
        let supported = cluster
            .effects
            .iter()
            .find(|candidate| candidate.id == effect.raw_id)
            .with_context(|| {
                format!(
                    "zone {} does not support effect {} (supported: {})",
                    cluster.index,
                    effect.name,
                    cluster
                        .effects
                        .iter()
                        .map(|effect| effect.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        rgb.set_sw_control(true, true)?;
        rgb.set_effect(cluster.index, supported.index, params, persistence)?;
    }
    Ok(zones)
}

#[derive(Debug, Clone)]
pub struct PerKeyApplied {
    pub scheme: ZoneScheme,
    pub keys: Vec<ResolvedKey>,
    pub zone_count: usize,
    pub requests: usize,
}

pub fn apply_perkey_pairs(
    target: &ManagedDevice,
    scheme: Option<ZoneScheme>,
    assignments: &[(String, String)],
    persistent: bool,
) -> Result<PerKeyApplied> {
    let scheme = perkey_scheme(target, scheme)?;
    let mut keys = Vec::with_capacity(assignments.len());
    let mut colors = Vec::with_capacity(assignments.len());
    for (key, color) in assignments {
        let resolved = resolve_key(key, scheme)?;
        validate_model_key(target, &resolved)?;
        colors.push((resolved.zone_id, color.parse::<RgbColor>()?));
        keys.push(resolved);
    }
    prepare_perkey(target)?;
    let requests = PerKeyLightingV2::new(&target.device)?.write_colors(&colors, persistent)?;
    Ok(PerKeyApplied {
        scheme,
        keys,
        zone_count: colors.len(),
        requests,
    })
}

pub fn apply_perkey_map(
    target: &ManagedDevice,
    assignments: &BTreeMap<String, String>,
    persistent: bool,
) -> Result<PerKeyApplied> {
    let assignments = assignments
        .iter()
        .map(|(key, color)| (key.clone(), color.clone()))
        .collect::<Vec<_>>();
    apply_perkey_pairs(target, None, &assignments, persistent)
}

pub fn apply_perkey_fill(
    target: &ManagedDevice,
    scheme: Option<ZoneScheme>,
    color: &str,
    persistent: bool,
) -> Result<PerKeyApplied> {
    let scheme = perkey_scheme(target, scheme)?;
    let map = target
        .model
        .and_then(|model| model.per_key_map.as_ref())
        .context(
            "this device has no embedded per-key usage map; use `perkey set` with explicit keys",
        )?;
    let usages = map
        .entries
        .keys()
        .map(|value| {
            value
                .parse::<u8>()
                .with_context(|| format!("invalid HID usage {value:?} in device registry"))
        })
        .collect::<Result<Vec<_>>>()?;
    let zones = zones_from_usages(usages, scheme)?;
    let color = color.parse::<RgbColor>()?;
    let colors = zones
        .iter()
        .copied()
        .map(|zone| (zone, color))
        .collect::<Vec<_>>();
    prepare_perkey(target)?;
    let requests = PerKeyLightingV2::new(&target.device)?.write_colors(&colors, persistent)?;
    Ok(PerKeyApplied {
        scheme,
        keys: Vec::new(),
        zone_count: colors.len(),
        requests,
    })
}

pub fn prepare_perkey(target: &ManagedDevice) -> Result<()> {
    let rgb = RgbEffects::new(&target.device)
        .context("per-key lighting requires the device's 0x8071 RGB control feature")?;
    let capabilities = rgb.capabilities()?;
    let cluster = capabilities
        .clusters
        .iter()
        .filter(|cluster| cluster.effects.iter().any(|effect| effect.id == 0x01))
        .find(|cluster| cluster.location & 0x0002 != 0)
        .or_else(|| {
            capabilities
                .clusters
                .iter()
                .find(|cluster| cluster.effects.iter().any(|effect| effect.id == 0x01))
        })
        .context("no RGB cluster supports the fixed effect required for per-key frames")?;
    let fixed = cluster
        .effects
        .iter()
        .find(|effect| effect.id == 0x01)
        .context("selected RGB cluster lost its fixed-effect capability")?;
    let params = encode_effect(
        Effect::parse("fixed")?,
        &EffectOptions {
            color: Some(RgbColor::BLACK),
            ..Default::default()
        },
    )?;
    rgb.set_sw_control(true, true)?;
    if let Err(error) = rgb.set_effect(cluster.index, fixed.index, params, Persistence::Ram) {
        let _ = rgb.set_sw_control(false, false);
        return Err(error);
    }
    Ok(())
}

fn perkey_scheme(target: &ManagedDevice, explicit: Option<ZoneScheme>) -> Result<ZoneScheme> {
    if let Some(scheme) = explicit {
        return Ok(scheme);
    }
    target
        .model
        .and_then(|model| model.per_key_map.as_ref())
        .and_then(|map| map.zone_scheme.as_deref())
        .map(str::parse)
        .transpose()?
        .context("per-key zone numbering is unresolved for this device; pass --zone-scheme hidusage|solaar or declare zone_scheme in data/devices.json")
}

fn validate_model_key(target: &ManagedDevice, key: &ResolvedKey) -> Result<()> {
    let Some(usage) = key.usage else {
        return Ok(());
    };
    let Some(map) = target.model.and_then(|model| model.per_key_map.as_ref()) else {
        return Ok(());
    };
    ensure!(
        map.entries.contains_key(&usage.to_string()),
        "key {} (HID usage 0x{usage:02X}) is not present in this device's per-key map",
        key.name
    );
    Ok(())
}

fn select_rgb_zones(capabilities: &RgbCapabilities, value: &str) -> Result<Vec<u8>> {
    if value.eq_ignore_ascii_case("all") || value.eq_ignore_ascii_case("ZONE_ALL") {
        return Ok(capabilities
            .clusters
            .iter()
            .map(|cluster| cluster.index)
            .collect());
    }
    if let Some(location) = ghub_zone_location(value) {
        let zones = capabilities
            .clusters
            .iter()
            .filter(|cluster| cluster.location == location)
            .map(|cluster| cluster.index)
            .collect::<Vec<_>>();
        ensure!(
            !zones.is_empty(),
            "RGB zone {value} (location 0x{location:04X}) is not reported by this device"
        );
        return Ok(zones);
    }
    let zone = parse_u8(value)?;
    ensure!(
        capabilities
            .clusters
            .iter()
            .any(|cluster| cluster.index == zone),
        "RGB zone {zone} does not exist (valid range: 0..{})",
        capabilities.device.cluster_count.saturating_sub(1)
    );
    Ok(vec![zone])
}

fn parse_u8(value: &str) -> Result<u8> {
    let parsed = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u8::from_str_radix(hex, 16)
    } else {
        value.parse::<u8>()
    };
    parsed.with_context(|| format!("invalid RGB zone {value:?}"))
}

fn ghub_zone_location(value: &str) -> Option<u16> {
    Some(match value.to_ascii_uppercase().as_str() {
        "ZONE_PRIMARY" => 2,
        "ZONE_LOGO" | "ZONE_BRANDING" => 4,
        "ZONE_ONE" => 8,
        "ZONE_TWO" => 16,
        "ZONE_THREE" => 32,
        "ZONE_FOUR" => 64,
        "ZONE_FIVE" => 128,
        "ZONE_SIX" => 256,
        "ZONE_SEVEN" => 512,
        "ZONE_LEFT_SIDE" => 1024,
        "ZONE_RIGHT_SIDE" => 2048,
        "ZONE_COMBINED" => 4096,
        "ZONE_TOP" => 8192,
        "ZONE_BOTTOM" => 16384,
        "ZONE_HALO" => 32768,
        "ZONE_IDLE_STATE" => 129,
        "ZONE_IN_USE_STATE" => 130,
        "ZONE_MUTED_STATE" => 131,
        "ZONE_SOFT_MUTED_STATE" => 132,
        "ZONE_FULL_SUPPORT" => 65535,
        _ => return None,
    })
}

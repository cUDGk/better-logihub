#![allow(dead_code)]

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::hidpp::device::Device;

const FEATURE_RGB_EFFECTS: u16 = 0x8071;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };
    pub const CYAN: Self = Self {
        r: 0,
        g: 0xFF,
        b: 0xFF,
    };

    fn scaled(self, percent: u8) -> Self {
        let scale = |value: u8| (u16::from(value) * u16::from(percent) / 100) as u8;
        Self {
            r: scale(self.r),
            g: scale(self.g),
            b: scale(self.b),
        }
    }
}

impl FromStr for RgbColor {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let value = value.strip_prefix('#').unwrap_or(value);
        ensure!(
            value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "color must be exactly six hexadecimal digits (RRGGBB)"
        );
        Ok(Self {
            r: u8::from_str_radix(&value[0..2], 16)?,
            g: u8::from_str_radix(&value[2..4], 16)?,
            b: u8::from_str_radix(&value[4..6], 16)?,
        })
    }
}

impl fmt::Display for RgbColor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    None,
    Fixed,
    Cycle,
    ColorWave,
    Breathing,
    Ripple,
    Custom,
    Kitt,
    Decomposition,
    FrameData,
    DualColor,
    ColorCycleS,
    ColorWaveS,
    RippleS,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effect {
    pub name: &'static str,
    pub raw_id: u16,
    layout: Layout,
}

impl Effect {
    pub fn parse(value: &str) -> Result<Self> {
        let normalized = value.to_ascii_lowercase().replace('-', "_");
        let (name, raw_id, layout) = match normalized.as_str() {
            "off" => ("off", 0x00, Layout::None),
            "fixed" => ("fixed", 0x01, Layout::Fixed),
            "cycle" => ("cycle", 0x03, Layout::Cycle),
            "colorwave" | "color_wave" => ("colorwave", 0x04, Layout::ColorWave),
            "breathing" | "breathe" => ("breathing", 0x0A, Layout::Breathing),
            "ripple" => ("ripple", 0x0B, Layout::Ripple),
            "custom" => ("custom", 0x0C, Layout::Custom),
            "kitt" => ("kitt", 0x0D, Layout::Kitt),
            "decomposition" => ("decomposition", 0x0E, Layout::Decomposition),
            "snipe_pulse_cp" | "signature_frame_active" => {
                ("signature_frame_active", 0x0F, Layout::FrameData)
            }
            "neural_wave_cp" | "signature_frame_inactive" => {
                ("signature_frame_inactive", 0x10, Layout::FrameData)
            }
            "snipe_pulse" => ("snipe_pulse", 0x11, Layout::DualColor),
            "neural_wave" => ("neural_wave", 0x12, Layout::DualColor),
            "color_cycle_s" | "cycle_s" => ("color_cycle_s", 0x15, Layout::ColorCycleS),
            "colorwave_s" | "color_wave_s" => ("colorwave_s", 0x16, Layout::ColorWaveS),
            "ripple_s" => ("ripple_s", 0x17, Layout::RippleS),
            "smooth_star" | "signature_algorithmic_active" => {
                ("signature_algorithmic_active", 0x18, Layout::DualColor)
            }
            "smooth_wave" | "signature_algorithmic_inactive" => {
                ("signature_algorithmic_inactive", 0x19, Layout::DualColor)
            }
            "signature_hardcoded_active" => ("signature_hardcoded_active", 0x1A, Layout::None),
            "signature_hardcoded_inactive" => ("signature_hardcoded_inactive", 0x1B, Layout::None),
            _ => bail!(
                "unknown RGB effect {value:?}; use off, fixed, breathing, cycle, colorwave, ripple, custom, kitt, decomposition, color_cycle_s, colorwave_s, ripple_s, or a signature effect name"
            ),
        };
        Ok(Self {
            name,
            raw_id,
            layout,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct EffectOptions {
    pub color: Option<RgbColor>,
    pub color2: Option<RgbColor>,
    pub speed: Option<u16>,
    pub period_ms: Option<u16>,
    pub brightness: Option<u8>,
    pub intensity: Option<u8>,
    pub direction: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Persistence {
    Ram,
    Nvm,
    PowerSave,
}

impl Persistence {
    pub const fn wire_value(self) -> u8 {
        match self {
            Self::Ram => 0x01,
            Self::Nvm => 0x02,
            // Power-saving effects are persistent and select the alternate bank.
            Self::PowerSave => 0x06,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RgbDeviceInfo {
    pub cluster_count: u8,
    pub nv_caps: u16,
    pub ext_caps: u16,
    pub extra: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RgbEffectInfo {
    pub index: u8,
    pub id: u16,
    pub name: String,
    pub capabilities: u16,
    pub period_ms: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RgbClusterInfo {
    pub index: u8,
    pub location: u16,
    pub effect_count: u8,
    pub persistence_caps: u8,
    pub effects: Vec<RgbEffectInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SwControl {
    pub enabled: bool,
    pub non_rgb: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RgbPowerModeConfig {
    pub value_1: u16,
    pub value_2: u16,
    pub value_3: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct RgbCapabilities {
    pub device: RgbDeviceInfo,
    pub clusters: Vec<RgbClusterInfo>,
    pub sw_control: SwControl,
    pub power_mode: u8,
    pub power_mode_config: RgbPowerModeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectSpecificInfo {
    State {
        value: u8,
        first: u16,
        second: u16,
    },
    Defaults {
        first: u16,
        second: u16,
        third: u16,
        value: u8,
    },
    Bytes {
        info_type: u8,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
pub enum RgbEvent {
    Sync(Vec<u8>),
    UserActivity(Vec<u8>),
    ClusterChanged(Vec<u8>),
    NvEffectSync(Vec<u8>),
}

impl RgbEvent {
    pub fn decode(index: u8, payload: &[u8]) -> Result<Self> {
        ensure!(
            payload.len() >= 4,
            "RGB event payload must contain at least 4 bytes"
        );
        let payload = payload.to_vec();
        match index {
            0 => Ok(Self::Sync(payload)),
            1 => Ok(Self::UserActivity(payload)),
            2 => Ok(Self::ClusterChanged(payload)),
            3 => Ok(Self::NvEffectSync(payload)),
            _ => bail!("unknown RGB event index {index}"),
        }
    }
}

pub struct RgbEffects<'a> {
    device: &'a Device,
    feature: u8,
}

impl<'a> RgbEffects<'a> {
    pub fn new(device: &'a Device) -> Result<Self> {
        let feature = device
            .require_feature(FEATURE_RGB_EFFECTS)
            .map_err(anyhow::Error::new)?;
        Ok(Self { device, feature })
    }

    pub fn capabilities(&self) -> Result<RgbCapabilities> {
        let device = self.device_info()?;
        let mut clusters = Vec::with_capacity(usize::from(device.cluster_count));
        for cluster in 0..device.cluster_count {
            clusters.push(self.cluster_info(cluster)?);
        }
        Ok(RgbCapabilities {
            device,
            clusters,
            sw_control: self.sw_control()?,
            power_mode: self.power_mode()?,
            power_mode_config: self.power_mode_config()?,
        })
    }

    pub fn device_info(&self) -> Result<RgbDeviceInfo> {
        let response = self.call(0, &device_info_request())?;
        Ok(parse_device_info(&response))
    }

    pub fn cluster_info(&self, cluster: u8) -> Result<RgbClusterInfo> {
        let response = self.call(0, &cluster_info_request(cluster))?;
        let (location, effect_count, persistence_caps) = parse_cluster_info(cluster, &response)?;
        let mut effects = Vec::with_capacity(usize::from(effect_count));
        for effect in 0..effect_count {
            effects.push(self.effect_info(cluster, effect)?);
        }
        Ok(RgbClusterInfo {
            index: cluster,
            location,
            effect_count,
            persistence_caps,
            effects,
        })
    }

    pub fn effect_info(&self, cluster: u8, effect: u8) -> Result<RgbEffectInfo> {
        let response = self.call(0, &effect_info_request(cluster, effect))?;
        Ok(parse_effect_info(effect, &response))
    }

    pub fn effect_specific_info(&self, cluster: u8, effect: u8) -> Result<Vec<EffectSpecificInfo>> {
        (0..=6)
            .map(|info_type| {
                let response = self.call(
                    0,
                    &effect_specific_info_request(cluster, effect, info_type)?,
                )?;
                parse_effect_specific_info(info_type, &response)
            })
            .collect()
    }

    pub fn set_effect(
        &self,
        cluster: u8,
        effect_index: u8,
        params: [u8; 10],
        persistence: Persistence,
    ) -> Result<()> {
        let request = set_effect_request(cluster, effect_index, params, persistence);
        self.call(1, &request)?;
        Ok(())
    }

    pub fn get_nv_config(&self, item: u16) -> Result<[u8; 7]> {
        let response = self.call(3, &get_nv_config_request(item))?;
        Ok(parse_nv_config(&response))
    }

    pub fn set_nv_config(&self, item: u16, value: [u8; 7]) -> Result<()> {
        self.call(3, &set_nv_config_request(item, value))?;
        Ok(())
    }

    pub fn sw_control(&self) -> Result<SwControl> {
        let response = self.call(5, &get_sw_control_request())?;
        Ok(parse_sw_control(&response))
    }

    pub fn set_sw_control(&self, enabled: bool, non_rgb: bool) -> Result<()> {
        self.call(5, &set_sw_control_request(enabled, non_rgb))?;
        Ok(())
    }

    pub fn power_mode_config(&self) -> Result<RgbPowerModeConfig> {
        let response = self.call(7, &get_power_mode_config_request())?;
        Ok(parse_power_mode_config(&response))
    }

    pub fn set_power_mode_config(&self, config: RgbPowerModeConfig) -> Result<()> {
        self.call(7, &set_power_mode_config_request(config))?;
        Ok(())
    }

    pub fn power_mode(&self) -> Result<u8> {
        Ok(parse_power_mode(&self.call(8, &get_power_mode_request())?))
    }

    pub fn set_power_mode(&self, mode: u8) -> Result<()> {
        self.call(8, &set_power_mode_request(mode))?;
        Ok(())
    }

    pub fn led_bin_info(&self, first: u8, second: u8) -> Result<[u16; 4]> {
        let response = self.call(4, &led_bin_info_request(first, second))?;
        Ok(parse_led_bin_info(&response))
    }

    pub fn set_multi_led_pattern(&self, first: u8, second: u8) -> Result<()> {
        self.call(2, &multi_led_pattern_request(first, second))?;
        Ok(())
    }

    pub fn set_effect_sync_correction(&self, cluster: u8, correction: u16) -> Result<()> {
        self.call(6, &sync_correction_request(cluster, correction))?;
        Ok(())
    }

    /// The specification confirms function 9 and its empty payload, but not its meaning.
    pub fn invoke_function_9(&self) -> Result<()> {
        self.call(9, &[])?;
        Ok(())
    }

    fn call(&self, function: u8, params: &[u8]) -> Result<[u8; 16]> {
        self.device
            .call_long(self.feature, function, params)
            .map_err(anyhow::Error::new)
    }
}

pub fn encode_effect(effect: Effect, options: &EffectOptions) -> Result<[u8; 10]> {
    let brightness = options.brightness.unwrap_or(100);
    let intensity = options.intensity.unwrap_or(100);
    ensure!(brightness <= 100, "brightness must be 0..=100");
    ensure!(intensity <= 100, "intensity must be 0..=100");

    let color = options.color.unwrap_or(RgbColor::CYAN);
    let color2 = options.color2.unwrap_or(RgbColor::BLACK);
    let scaled_color = color.scaled(brightness);
    let wire_intensity = ((u16::from(intensity) * u16::from(brightness) / 100) as u8).max(1);
    let mut params = [0_u8; 10];

    match effect.layout {
        Layout::None => {}
        Layout::Fixed => {
            params[..3].copy_from_slice(&[scaled_color.r, scaled_color.g, scaled_color.b]);
            params[3] = 2;
        }
        Layout::Cycle => {
            let period = effect_period(effect, options)?;
            params[5..7].copy_from_slice(&period.to_be_bytes());
            params[7] = wire_intensity;
        }
        Layout::ColorWave => {
            let period = effect_period(effect, options)?;
            let [high, low] = period.to_be_bytes();
            params[6] = low;
            params[7] = direction(options)?;
            params[8] = wire_intensity;
            params[9] = high;
        }
        Layout::Breathing => {
            params[..3].copy_from_slice(&[color.r, color.g, color.b]);
            params[3..5].copy_from_slice(&effect_period(effect, options)?.to_be_bytes());
            params[5] = 0;
            params[6] = wire_intensity;
        }
        Layout::Ripple => {
            params[..3].copy_from_slice(&[scaled_color.r, scaled_color.g, scaled_color.b]);
            params[3] = 0;
            params[4..6].copy_from_slice(&effect_period(effect, options)?.to_be_bytes());
        }
        Layout::Custom => {
            params[0] = 0;
            params[5..7].copy_from_slice(&options.speed.unwrap_or(32).to_be_bytes());
            params[7] = wire_intensity;
        }
        Layout::Kitt => {
            params[..3].copy_from_slice(&[color.r, color.g, color.b]);
            params[3] = 100;
            params[4..6].copy_from_slice(&effect_period(effect, options)?.to_be_bytes());
            params[6..9].copy_from_slice(&[color2.r, color2.g, color2.b]);
            params[9] = wire_intensity;
        }
        Layout::Decomposition => {
            let period = effect_period(effect, options)?;
            let [high, low] = period.to_be_bytes();
            params[6] = low;
            params[7] = high;
            params[8] = wire_intensity;
        }
        Layout::FrameData => {
            params[5..7].copy_from_slice(&options.speed.unwrap_or(32).to_be_bytes());
            params[7] = wire_intensity;
        }
        Layout::DualColor => {
            params[..3].copy_from_slice(&[color.r, color.g, color.b]);
            params[3..6].copy_from_slice(&[color2.r, color2.g, color2.b]);
            params[6..8].copy_from_slice(&effect_period(effect, options)?.to_be_bytes());
            params[8] = wire_intensity;
            params[9] = 0;
        }
        Layout::ColorCycleS => {
            params[1] = 0xFF;
            params[6..8].copy_from_slice(&effect_period(effect, options)?.to_be_bytes());
            params[8] = wire_intensity;
        }
        Layout::ColorWaveS => {
            params[1] = 0xFF;
            params[6..8].copy_from_slice(&effect_period(effect, options)?.to_be_bytes());
            params[8] = wire_intensity;
            params[9] = direction(options)?;
        }
        Layout::RippleS => {
            params[..3].copy_from_slice(&[scaled_color.r, scaled_color.g, scaled_color.b]);
            params[3] = 0xFF;
            params[4] = 0;
            params[6..8].copy_from_slice(&effect_period(effect, options)?.to_be_bytes());
        }
    }
    Ok(params)
}

pub fn parse_direction(value: &str) -> Result<u8> {
    let normalized = value.to_ascii_lowercase().replace('-', "_");
    match normalized.as_str() {
        "horizontal" => Ok(1),
        "vertical" => Ok(2),
        "center_out" => Ok(3),
        "inward" => Ok(4),
        "outward" => Ok(5),
        "reverse_horizontal" => Ok(6),
        "reverse_vertical" => Ok(7),
        "center_in" => Ok(8),
        _ => value
            .parse::<u8>()
            .ok()
            .filter(|value| (1..=8).contains(value))
            .context("direction must be horizontal, vertical, center-out, inward, outward, reverse-horizontal, reverse-vertical, center-in, or 1..=8"),
    }
}

fn direction(options: &EffectOptions) -> Result<u8> {
    let direction = options.direction.unwrap_or(1);
    ensure!((1..=8).contains(&direction), "direction must be 1..=8");
    Ok(direction)
}

fn effect_period(effect: Effect, options: &EffectOptions) -> Result<u16> {
    let default = match effect.layout {
        Layout::Ripple | Layout::RippleS => 20,
        Layout::DualColor => 3000,
        _ => 5000,
    };
    let period = options.period_ms.unwrap_or(default);
    let valid = match effect.layout {
        Layout::Ripple | Layout::RippleS => (2..=200).contains(&period),
        _ => (1000..=20_000).contains(&period),
    };
    ensure!(
        valid,
        "period for {} is outside the supported UI range",
        effect.name
    );
    Ok(period)
}

fn device_info_request() -> [u8; 3] {
    [0xFF, 0xFF, 0]
}

fn cluster_info_request(cluster: u8) -> [u8; 3] {
    [cluster, 0xFF, 0]
}

fn effect_info_request(cluster: u8, effect: u8) -> [u8; 3] {
    [cluster, effect, 0]
}

fn effect_specific_info_request(cluster: u8, effect: u8, info_type: u8) -> Result<[u8; 5]> {
    ensure!(info_type <= 6, "effect-specific info type must be 0..=6");
    Ok([0xFF, cluster, 1, effect, info_type])
}

fn get_nv_config_request(item: u16) -> [u8; 3] {
    let [high, low] = item.to_be_bytes();
    [0, high, low]
}

fn set_nv_config_request(item: u16, value: [u8; 7]) -> [u8; 10] {
    let mut request = [0_u8; 10];
    request[0] = 1;
    request[1..3].copy_from_slice(&item.to_be_bytes());
    request[3..].copy_from_slice(&value);
    request
}

fn parse_nv_config(response: &[u8; 16]) -> [u8; 7] {
    response[3..10].try_into().unwrap()
}

fn get_sw_control_request() -> [u8; 3] {
    [0, 0, 0]
}

fn set_sw_control_request(enabled: bool, non_rgb: bool) -> [u8; 3] {
    [1, u8::from(enabled), u8::from(non_rgb)]
}

fn parse_sw_control(response: &[u8; 16]) -> SwControl {
    SwControl {
        enabled: response[1] != 0,
        non_rgb: response[2] != 0,
    }
}

fn led_bin_info_request(first: u8, second: u8) -> [u8; 3] {
    [0, first, second]
}

fn parse_led_bin_info(response: &[u8; 16]) -> [u16; 4] {
    [
        u16::from_be_bytes([response[3], response[4]]),
        u16::from_be_bytes([response[5], response[6]]),
        u16::from_be_bytes([response[7], response[8]]),
        u16::from_be_bytes([response[9], response[10]]),
    ]
}

fn multi_led_pattern_request(first: u8, second: u8) -> [u8; 2] {
    [first, second]
}

fn get_power_mode_config_request() -> [u8; 6] {
    [0; 6]
}

fn set_power_mode_config_request(config: RgbPowerModeConfig) -> [u8; 7] {
    let mut request = [0_u8; 7];
    request[0] = 1;
    request[1..3].copy_from_slice(&config.value_1.to_be_bytes());
    request[3..5].copy_from_slice(&config.value_2.to_be_bytes());
    request[5..7].copy_from_slice(&config.value_3.to_be_bytes());
    request
}

fn parse_power_mode_config(response: &[u8; 16]) -> RgbPowerModeConfig {
    RgbPowerModeConfig {
        value_1: u16::from_be_bytes([response[1], response[2]]),
        value_2: u16::from_be_bytes([response[3], response[4]]),
        value_3: u16::from_be_bytes([response[5], response[6]]),
    }
}

fn get_power_mode_request() -> [u8; 1] {
    [0]
}

fn set_power_mode_request(mode: u8) -> [u8; 2] {
    [1, mode]
}

fn parse_power_mode(response: &[u8; 16]) -> u8 {
    response[1]
}

fn sync_correction_request(cluster: u8, correction: u16) -> [u8; 4] {
    let [high, low] = correction.to_be_bytes();
    [cluster, 0, high, low]
}

fn set_effect_request(
    cluster: u8,
    effect_index: u8,
    params: [u8; 10],
    persistence: Persistence,
) -> [u8; 13] {
    let mut request = [0_u8; 13];
    request[0] = cluster;
    request[1] = effect_index;
    request[2..12].copy_from_slice(&params);
    request[12] = persistence.wire_value();
    request
}

fn parse_device_info(response: &[u8; 16]) -> RgbDeviceInfo {
    RgbDeviceInfo {
        cluster_count: response[2],
        nv_caps: u16::from_be_bytes([response[3], response[4]]),
        ext_caps: u16::from_be_bytes([response[5], response[6]]),
        extra: response[7],
    }
}

fn parse_cluster_info(cluster: u8, response: &[u8; 16]) -> Result<(u16, u8, u8)> {
    ensure!(
        response[0] == cluster,
        "RGB cluster response echoed the wrong index"
    );
    Ok((
        u16::from_be_bytes([response[2], response[3]]),
        response[4],
        response[5],
    ))
}

fn parse_effect_info(index: u8, response: &[u8; 16]) -> RgbEffectInfo {
    let id = u16::from_be_bytes([response[2], response[3]]);
    RgbEffectInfo {
        index,
        id,
        name: raw_effect_name(id).to_owned(),
        capabilities: u16::from_be_bytes([response[4], response[5]]),
        period_ms: u16::from_be_bytes([response[6], response[7]]),
    }
}

fn parse_effect_specific_info(info_type: u8, response: &[u8; 16]) -> Result<EffectSpecificInfo> {
    ensure!(
        response[0] == info_type,
        "effect-specific response echoed the wrong type"
    );
    match info_type {
        0 => Ok(EffectSpecificInfo::State {
            value: response[1],
            first: u16::from_be_bytes([response[2], response[3]]),
            second: u16::from_be_bytes([response[4], response[5]]),
        }),
        1 => Ok(EffectSpecificInfo::Defaults {
            first: u16::from_be_bytes([response[1], response[2]]),
            second: u16::from_be_bytes([response[3], response[4]]),
            third: u16::from_be_bytes([response[5], response[6]]),
            value: response[7],
        }),
        2 | 4 | 5 | 6 => Ok(EffectSpecificInfo::Bytes {
            info_type,
            bytes: response[1..12].to_vec(),
        }),
        3 => Ok(EffectSpecificInfo::Bytes {
            info_type,
            bytes: response[1..7].to_vec(),
        }),
        _ => bail!("effect-specific info type must be 0..=6"),
    }
}

fn raw_effect_name(id: u16) -> &'static str {
    match id {
        0x00 => "off",
        0x01 => "fixed",
        0x03 => "cycle",
        0x04 => "colorwave",
        0x0A => "breathing",
        0x0B => "ripple",
        0x0C => "custom",
        0x0D => "kitt",
        0x0E => "decomposition",
        0x0F => "signature_frame_active",
        0x10 => "signature_frame_inactive",
        0x11 => "snipe_pulse",
        0x12 => "neural_wave",
        0x13 => "streaming",
        0x15 => "color_cycle_s",
        0x16 => "colorwave_s",
        0x17 => "ripple_s",
        0x18 => "signature_algorithmic_active",
        0x19 => "signature_algorithmic_inactive",
        0x1A => "signature_hardcoded_active",
        0x1B => "signature_hardcoded_inactive",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effect(name: &str) -> Effect {
        Effect::parse(name).unwrap()
    }

    fn options() -> EffectOptions {
        EffectOptions {
            color: Some(RgbColor { r: 1, g: 2, b: 3 }),
            color2: Some(RgbColor { r: 4, g: 5, b: 6 }),
            speed: Some(0x1234),
            period_ms: Some(0x1234),
            brightness: Some(100),
            intensity: Some(100),
            direction: Some(2),
        }
    }

    #[test]
    fn maps_proto_names_to_raw_effect_ids() {
        assert_eq!(effect("OFF").raw_id, 0x00);
        assert_eq!(effect("fixed").raw_id, 0x01);
        assert_eq!(effect("COLORWAVE_S").raw_id, 0x16);
        assert_eq!(effect("signature-algorithmic-active").raw_id, 0x18);
    }

    #[test]
    fn encodes_every_effect_parameter_layout_from_the_spec_table() {
        let options = options();
        let mut ripple_options = options.clone();
        ripple_options.period_ms = Some(0x00C8);
        assert_eq!(encode_effect(effect("off"), &options).unwrap(), [0; 10]);
        assert_eq!(
            encode_effect(effect("fixed"), &options).unwrap(),
            [1, 2, 3, 2, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            encode_effect(effect("cycle"), &options).unwrap(),
            [0, 0, 0, 0, 0, 0x12, 0x34, 100, 0, 0]
        );
        assert_eq!(
            encode_effect(effect("colorwave"), &options).unwrap(),
            [0, 0, 0, 0, 0, 0, 0x34, 2, 100, 0x12]
        );
        assert_eq!(
            encode_effect(effect("breathing"), &options).unwrap(),
            [1, 2, 3, 0x12, 0x34, 0, 100, 0, 0, 0]
        );
        assert_eq!(
            encode_effect(effect("ripple"), &ripple_options).unwrap(),
            [1, 2, 3, 0, 0x00, 0xC8, 0, 0, 0, 0]
        );
        assert_eq!(
            encode_effect(effect("custom"), &options).unwrap(),
            [0, 0, 0, 0, 0, 0x12, 0x34, 100, 0, 0]
        );
        assert_eq!(
            encode_effect(effect("kitt"), &options).unwrap(),
            [1, 2, 3, 100, 0x12, 0x34, 4, 5, 6, 100]
        );
        assert_eq!(
            encode_effect(effect("decomposition"), &options).unwrap(),
            [0, 0, 0, 0, 0, 0, 0x34, 0x12, 100, 0]
        );
        assert_eq!(
            encode_effect(effect("signature_frame_active"), &options).unwrap(),
            [0, 0, 0, 0, 0, 0x12, 0x34, 100, 0, 0]
        );
        assert_eq!(
            encode_effect(effect("snipe_pulse"), &options).unwrap(),
            [1, 2, 3, 4, 5, 6, 0x12, 0x34, 100, 0]
        );
        assert_eq!(
            encode_effect(effect("color_cycle_s"), &options).unwrap(),
            [0, 0xFF, 0, 0, 0, 0, 0x12, 0x34, 100, 0]
        );
        assert_eq!(
            encode_effect(effect("colorwave_s"), &options).unwrap(),
            [0, 0xFF, 0, 0, 0, 0, 0x12, 0x34, 100, 2]
        );
        assert_eq!(
            encode_effect(effect("ripple_s"), &ripple_options).unwrap(),
            [1, 2, 3, 0xFF, 0, 0, 0x00, 0xC8, 0, 0]
        );
    }

    #[test]
    fn scales_only_the_specified_color_and_intensity_fields() {
        let options = EffectOptions {
            color: Some(RgbColor {
                r: 200,
                g: 100,
                b: 50,
            }),
            brightness: Some(50),
            intensity: Some(50),
            ..Default::default()
        };
        assert_eq!(
            &encode_effect(effect("fixed"), &options).unwrap()[..4],
            &[100, 50, 25, 2]
        );
        let breathing = encode_effect(effect("breathing"), &options).unwrap();
        assert_eq!(&breathing[..3], &[200, 100, 50]);
        assert_eq!(breathing[6], 25);
    }

    #[test]
    fn builds_exact_thirteen_byte_set_effect_request() {
        assert_eq!(
            set_effect_request(2, 3, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], Persistence::Ram),
            [2, 3, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 1]
        );
        assert_eq!(Persistence::Nvm.wire_value(), 2);
        assert_eq!(Persistence::PowerSave.wire_value(), 6);
    }

    #[test]
    fn encodes_management_requests_with_selectors_and_big_endian_fields() {
        assert_eq!(device_info_request(), [0xFF, 0xFF, 0]);
        assert_eq!(cluster_info_request(2), [2, 0xFF, 0]);
        assert_eq!(effect_info_request(2, 3), [2, 3, 0]);
        assert_eq!(
            effect_specific_info_request(2, 3, 6).unwrap(),
            [0xFF, 2, 1, 3, 6]
        );
        assert_eq!(get_nv_config_request(0x0040), [0, 0, 0x40]);
        assert_eq!(
            set_nv_config_request(0x0040, [1, 2, 3, 4, 5, 6, 7]),
            [1, 0, 0x40, 1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(get_sw_control_request(), [0, 0, 0]);
        assert_eq!(set_sw_control_request(true, false), [1, 1, 0]);
        assert_eq!(led_bin_info_request(4, 5), [0, 4, 5]);
        assert_eq!(multi_led_pattern_request(4, 5), [4, 5]);
        assert_eq!(get_power_mode_config_request(), [0; 6]);
        assert_eq!(
            set_power_mode_config_request(RgbPowerModeConfig {
                value_1: 0x1234,
                value_2: 0x5678,
                value_3: 0x9ABC,
            }),
            [1, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]
        );
        assert_eq!(get_power_mode_request(), [0]);
        assert_eq!(set_power_mode_request(5), [1, 5]);
        assert_eq!(sync_correction_request(2, 0x1234), [2, 0, 0x12, 0x34]);
    }

    #[test]
    fn decodes_management_response_fixtures() {
        let mut response = [0_u8; 16];
        response[1..3].copy_from_slice(&[1, 0]);
        assert_eq!(
            parse_sw_control(&response),
            SwControl {
                enabled: true,
                non_rgb: false,
            }
        );
        response[1] = 5;
        assert_eq!(parse_power_mode(&response), 5);

        response[1..7].copy_from_slice(&[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]);
        assert_eq!(
            parse_power_mode_config(&response),
            RgbPowerModeConfig {
                value_1: 0x1234,
                value_2: 0x5678,
                value_3: 0x9ABC,
            }
        );
        response[3..11].copy_from_slice(&[0x00, 1, 0x00, 2, 0x00, 3, 0x00, 4]);
        assert_eq!(parse_led_bin_info(&response), [1, 2, 3, 4]);
        response[3..10].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(parse_nv_config(&response), [1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn decodes_device_effect_and_specific_info_fixtures() {
        let mut device = [0_u8; 16];
        device[..8].copy_from_slice(&[0xFF, 0xFF, 2, 0x00, 0x41, 0x12, 0x34, 0xAA]);
        assert_eq!(
            parse_device_info(&device),
            RgbDeviceInfo {
                cluster_count: 2,
                nv_caps: 0x0041,
                ext_caps: 0x1234,
                extra: 0xAA,
            }
        );

        let mut cluster = [0_u8; 16];
        cluster[..6].copy_from_slice(&[1, 0, 0x00, 0x04, 6, 3]);
        assert_eq!(parse_cluster_info(1, &cluster).unwrap(), (4, 6, 3));

        let mut effect = [0_u8; 16];
        effect[2..8].copy_from_slice(&[0x00, 0x04, 0xAB, 0xCD, 0x13, 0x88]);
        let decoded = parse_effect_info(7, &effect);
        assert_eq!(
            (
                decoded.index,
                decoded.id,
                decoded.capabilities,
                decoded.period_ms
            ),
            (7, 4, 0xABCD, 5000)
        );

        let mut state = [0_u8; 16];
        state[..8].copy_from_slice(&[0, 3, 0x12, 0x34, 0xAB, 0xCD, 0, 0]);
        assert_eq!(
            parse_effect_specific_info(0, &state).unwrap(),
            EffectSpecificInfo::State {
                value: 3,
                first: 0x1234,
                second: 0xABCD,
            }
        );
    }

    #[test]
    fn validates_event_minimum_length_and_direction_names() {
        assert!(RgbEvent::decode(0, &[1, 2, 3]).is_err());
        assert!(matches!(
            RgbEvent::decode(2, &[1, 2, 3, 4]).unwrap(),
            RgbEvent::ClusterChanged(_)
        ));
        assert_eq!(parse_direction("reverse-horizontal").unwrap(), 6);
    }
}

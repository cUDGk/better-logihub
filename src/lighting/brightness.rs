#![allow(dead_code)]

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::hidpp::device::Device;

const FEATURE_BRIGHTNESS_CONTROL: u16 = 0x8040;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BrightnessInfo {
    pub max: u16,
    pub min: u16,
    /// G HUB's exact `(response[6] << 8) | response[2]` interpretation.
    pub steps: u16,
    /// Conservative public interpretation of the otherwise ambiguous field.
    pub safe_steps: u8,
    pub capabilities: u8,
    pub raw_byte_6: u8,
}

pub struct Brightness<'a> {
    device: &'a Device,
    feature: u8,
}

impl<'a> Brightness<'a> {
    pub fn new(device: &'a Device) -> Result<Self> {
        let feature = device
            .require_feature(FEATURE_BRIGHTNESS_CONTROL)
            .map_err(anyhow::Error::new)?;
        Ok(Self { device, feature })
    }

    pub fn info(&self) -> Result<BrightnessInfo> {
        let response = self
            .device
            .call_long(self.feature, 0, &[])
            .map_err(anyhow::Error::new)?;
        Ok(parse_info(&response))
    }

    pub fn brightness(&self) -> Result<u16> {
        let response = self
            .device
            .call_long(self.feature, 1, &[])
            .map_err(anyhow::Error::new)?;
        Ok(parse_brightness(&response))
    }

    pub fn set_brightness(&self, level: u16) -> Result<u16> {
        self.device
            .call_long(self.feature, 2, &brightness_request(level))
            .map_err(anyhow::Error::new)?;
        self.brightness()
    }

    pub fn illumination(&self) -> Result<bool> {
        let response = self
            .device
            .call_long(self.feature, 3, &[])
            .map_err(anyhow::Error::new)?;
        Ok(parse_illumination(&response))
    }

    pub fn set_illumination(&self, enabled: bool) -> Result<bool> {
        self.device
            .call_long(self.feature, 4, &illumination_request(enabled))
            .map_err(anyhow::Error::new)?;
        self.illumination()
    }
}

pub fn raw_from_percent(info: BrightnessInfo, percent: u8) -> Result<u16> {
    ensure!(percent <= 100, "brightness percentage must be 0..=100");
    ensure!(
        info.max >= info.min,
        "device reports max brightness below min brightness"
    );
    let span = u32::from(info.max - info.min);
    Ok(info.min + ((span * u32::from(percent) + 50) / 100) as u16)
}

pub fn percent_from_raw(info: BrightnessInfo, level: u16) -> u8 {
    if info.max <= info.min {
        return 0;
    }
    let level = level.clamp(info.min, info.max) - info.min;
    ((u32::from(level) * 100 + u32::from(info.max - info.min) / 2) / u32::from(info.max - info.min))
        as u8
}

fn parse_info(response: &[u8; 16]) -> BrightnessInfo {
    BrightnessInfo {
        max: u16::from_be_bytes([response[0], response[1]]),
        min: u16::from_be_bytes([response[4], response[5]]),
        steps: u16::from_be_bytes([response[6], response[2]]),
        safe_steps: response[2] & 0x0F,
        capabilities: response[3],
        raw_byte_6: response[6],
    }
}

fn brightness_request(level: u16) -> [u8; 2] {
    level.to_be_bytes()
}

fn parse_brightness(response: &[u8; 16]) -> u16 {
    u16::from_be_bytes([response[0], response[1]])
}

fn parse_illumination(response: &[u8; 16]) -> bool {
    response[0] & 1 != 0
}

fn illumination_request(enabled: bool) -> [u8; 1] {
    [u8::from(enabled)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_get_info_fixture_including_ambiguous_byte_six() {
        let mut response = [0_u8; 16];
        response[..7].copy_from_slice(&[0x03, 0xE8, 0x0A, 0x04, 0x00, 0x64, 0x01]);
        assert_eq!(
            parse_info(&response),
            BrightnessInfo {
                max: 1000,
                min: 100,
                steps: 0x010A,
                safe_steps: 10,
                capabilities: 4,
                raw_byte_6: 1,
            }
        );
    }

    #[test]
    fn encodes_brightness_as_big_endian_and_converts_percent() {
        assert_eq!(brightness_request(0x1234), [0x12, 0x34]);
        let mut response = [0_u8; 16];
        response[..2].copy_from_slice(&[0x12, 0x34]);
        assert_eq!(parse_brightness(&response), 0x1234);
        response[0] = 3;
        assert!(parse_illumination(&response));
        assert_eq!(illumination_request(false), [0]);
        assert_eq!(illumination_request(true), [1]);
        let info = BrightnessInfo {
            max: 1000,
            min: 100,
            steps: 10,
            safe_steps: 10,
            capabilities: 4,
            raw_byte_6: 0,
        };
        assert_eq!(raw_from_percent(info, 50).unwrap(), 550);
        assert_eq!(percent_from_raw(info, 550), 50);
        assert!(raw_from_percent(info, 101).is_err());
    }
}

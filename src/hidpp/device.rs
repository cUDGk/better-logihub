use std::rc::Rc;

use serde::Serialize;

use super::transport::{HidTransport, HidppError, dpi_from_be, dpi_to_be, fn_sw, long_frame};

const FEATURE_ROOT: u16 = 0x0000;
const FEATURE_SET: u16 = 0x0001;
const FEATURE_DEVICE_NAME: u16 = 0x0005;
const FEATURE_BATTERY_LEVEL_STATUS: u16 = 0x1000;
const FEATURE_UNIFIED_BATTERY: u16 = 0x1004;
const FEATURE_ADJUSTABLE_DPI: u16 = 0x2201;
const FEATURE_EXTENDED_ADJUSTABLE_DPI: u16 = 0x2202;
const FEATURE_REPORT_RATE: u16 = 0x8060;
const FEATURE_EXTENDED_REPORT_RATE: u16 = 0x8061;
const FEATURE_ONBOARD_PROFILES: u16 = 0x8100;

#[derive(Debug, Clone, Serialize)]
pub struct BatteryStatus {
    pub percent: u8,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeatureInfo {
    pub index: u8,
    pub id: u16,
    pub name: &'static str,
}

pub struct Device {
    transport: Rc<HidTransport>,
    dev_idx: u8,
}

impl Device {
    pub fn new(transport: Rc<HidTransport>, dev_idx: u8) -> Self {
        Self { transport, dev_idx }
    }

    pub fn probe(&self) -> Result<(), HidppError> {
        self.require_feature(FEATURE_SET).map(|_| ())
    }

    pub fn name(&self) -> Result<String, HidppError> {
        let feature = self.require_feature(FEATURE_DEVICE_NAME)?;
        let length = self.call(feature, 0, &[])?[0] as usize;
        let mut name = Vec::with_capacity(length);
        while name.len() < length {
            let offset = u8::try_from(name.len()).map_err(|_| {
                HidppError::Malformed("device name is longer than 255 bytes".into())
            })?;
            let response = self.call(feature, 1, &[offset])?;
            let needed = length - name.len();
            let chunk_length = needed.min(response.len());
            let chunk = &response[..chunk_length];
            let before = name.len();
            name.extend(chunk.iter().copied().take_while(|byte| *byte != 0));
            if name.len() == before {
                return Err(HidppError::Malformed(
                    "device returned an empty name chunk".into(),
                ));
            }
        }
        name.truncate(length);
        Ok(String::from_utf8_lossy(&name).into_owned())
    }

    pub fn battery(&self) -> Result<BatteryStatus, HidppError> {
        if let Some(feature) = self.get_feature(FEATURE_UNIFIED_BATTERY)? {
            let response = self.call(feature, 1, &[])?;
            return Ok(BatteryStatus {
                percent: response[0],
                status: unified_battery_status_name(response[2]).to_owned(),
            });
        }
        let feature = self.require_feature(FEATURE_BATTERY_LEVEL_STATUS)?;
        let response = self.call(feature, 0, &[])?;
        Ok(BatteryStatus {
            percent: response[0],
            status: battery_level_status_name(response[2]).to_owned(),
        })
    }

    pub fn dpi(&self) -> Result<u16, HidppError> {
        if let Some(feature) = self.get_feature(FEATURE_EXTENDED_ADJUSTABLE_DPI)? {
            let response = self.call(feature, 5, &[0])?;
            return Ok(dpi_from_be(response[1], response[2]));
        }
        let feature = self.require_feature(FEATURE_ADJUSTABLE_DPI)?;
        let response = self.call(feature, 2, &[0])?;
        Ok(dpi_from_be(response[1], response[2]))
    }

    pub fn set_dpi(&self, dpi: u16) -> Result<(), HidppError> {
        let [high, low] = dpi_to_be(dpi);
        if let Some(feature) = self.get_feature(FEATURE_EXTENDED_ADJUSTABLE_DPI)? {
            self.call(feature, 6, &[0, high, low, high, low])?;
            return Ok(());
        }
        let feature = self.require_feature(FEATURE_ADJUSTABLE_DPI)?;
        self.call(feature, 3, &[0, high, low])?;
        Ok(())
    }

    pub fn report_rate(&self) -> Result<u32, HidppError> {
        if let Some(feature) = self.get_feature(FEATURE_EXTENDED_REPORT_RATE)? {
            let value = self.call(feature, 1, &[])?[0];
            return extended_rate_to_hz(value).ok_or_else(|| {
                HidppError::Malformed(format!("unknown extended report-rate value {value}"))
            });
        }
        let feature = self.require_feature(FEATURE_REPORT_RATE)?;
        let interval_ms = self.call(feature, 1, &[])?[0];
        if interval_ms == 0 {
            return Err(HidppError::Malformed(
                "device returned a zero report-rate interval".into(),
            ));
        }
        Ok(1000 / u32::from(interval_ms))
    }

    pub fn set_report_rate(&self, hz: u32) -> Result<(), HidppError> {
        let (feature, value) = if let Some(feature) =
            self.get_feature(FEATURE_EXTENDED_REPORT_RATE)?
        {
            let value = hz_to_extended_rate(hz).ok_or_else(|| {
                HidppError::Malformed(format!(
                    "{hz} Hz is unsupported; valid extended rates are 125, 250, 500, 1000, 2000, 4000, and 8000 Hz"
                ))
            })?;
            (feature, value)
        } else {
            let feature = self.require_feature(FEATURE_REPORT_RATE)?;
            if hz == 0 || 1000 % hz != 0 || 1000 / hz > u8::MAX as u32 {
                return Err(HidppError::Malformed(format!(
                    "{hz} Hz cannot be represented as a whole millisecond interval"
                )));
            }
            (feature, (1000 / hz) as u8)
        };

        match self.call(feature, 2, &[value]) {
            Ok(_) => Ok(()),
            Err(initial) if initial.is_protocol_error() => {
                let onboard = self.require_feature(FEATURE_ONBOARD_PROFILES)?;
                self.call(onboard, 1, &[0x02])?;
                self.call(feature, 2, &[value])?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub fn features(&self) -> Result<Vec<FeatureInfo>, HidppError> {
        let feature_set = self.require_feature(FEATURE_SET)?;
        let count = self.call(feature_set, 0, &[])?[0];
        let mut features = Vec::with_capacity(usize::from(count) + 1);
        features.push(FeatureInfo {
            index: 0,
            id: FEATURE_ROOT,
            name: feature_name(FEATURE_ROOT),
        });
        for index in 1..=count {
            let response = self.call(feature_set, 1, &[index])?;
            let id = u16::from_be_bytes([response[0], response[1]]);
            features.push(FeatureInfo {
                index,
                id,
                name: feature_name(id),
            });
        }
        Ok(features)
    }

    fn require_feature(&self, feature_id: u16) -> Result<u8, HidppError> {
        self.get_feature(feature_id)?
            .ok_or(HidppError::UnsupportedFeature(feature_id))
    }

    fn get_feature(&self, feature_id: u16) -> Result<Option<u8>, HidppError> {
        let params = feature_id.to_be_bytes();
        let response = self.call(0, 0, &params)?;
        Ok((response[0] != 0).then_some(response[0]))
    }

    fn call(&self, feature_idx: u8, function: u8, params: &[u8]) -> Result<[u8; 16], HidppError> {
        let request = long_frame(self.dev_idx, feature_idx, fn_sw(function), params);
        let response = self.transport.transact(&request)?;
        let mut result = [0_u8; 16];
        result.copy_from_slice(response.params());
        Ok(result)
    }
}

fn unified_battery_status_name(value: u8) -> &'static str {
    match value {
        0 => "discharging",
        1 => "charging",
        2 => "charging_slow",
        3 => "full",
        4 => "error",
        _ => "unknown",
    }
}

fn battery_level_status_name(value: u8) -> &'static str {
    match value {
        0 => "discharging",
        1 => "charging",
        2 => "charging (final stage)",
        3 => "full",
        4 => "charging (below optimal speed)",
        5 => "invalid battery",
        7 => "thermal error",
        8 => "charging error",
        _ => "unknown",
    }
}

fn extended_rate_to_hz(value: u8) -> Option<u32> {
    match value {
        0 => Some(125),
        1 => Some(250),
        2 => Some(500),
        3 => Some(1000),
        4 => Some(2000),
        5 => Some(4000),
        6 => Some(8000),
        _ => None,
    }
}

fn hz_to_extended_rate(hz: u32) -> Option<u8> {
    match hz {
        125 => Some(0),
        250 => Some(1),
        500 => Some(2),
        1000 => Some(3),
        2000 => Some(4),
        4000 => Some(5),
        8000 => Some(6),
        _ => None,
    }
}

pub fn feature_name(id: u16) -> &'static str {
    match id {
        0x0000 => "IRoot",
        0x0001 => "IFeatureSet",
        0x0003 => "IFirmwareInfo",
        0x0005 => "DeviceName",
        0x1000 => "BatteryLevelStatus",
        0x1001 => "BatteryVoltage",
        0x1004 => "UnifiedBattery",
        0x1802 => "DeviceReset",
        0x1B04 => "WirelessDeviceStatus",
        0x1DF3 => "EquadDjDebugInfo",
        0x1E00 => "EnableHiddenFeatures",
        0x1F03 => "DeviceFriendlyName",
        0x2200 => "MousePointer",
        0x2201 => "AdjustableDPI",
        0x2202 => "ExtendedAdjustableDPI",
        0x8060 => "ReportRate",
        0x8061 => "ExtendedReportRate",
        0x8070 => "ColorLedEffects",
        0x8071 => "RgbEffects",
        0x8080 => "PerKeyLighting",
        0x8100 => "OnboardProfiles",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_extended_report_rates() {
        assert_eq!(extended_rate_to_hz(0), Some(125));
        assert_eq!(extended_rate_to_hz(6), Some(8000));
        assert_eq!(hz_to_extended_rate(1000), Some(3));
        assert_eq!(hz_to_extended_rate(144), None);
    }

    #[test]
    fn converts_unified_battery_status_separately() {
        assert_eq!(unified_battery_status_name(0), "discharging");
        assert_eq!(unified_battery_status_name(2), "charging_slow");
        assert_eq!(unified_battery_status_name(3), "full");
        assert_eq!(unified_battery_status_name(4), "error");
    }
}

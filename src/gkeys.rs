use anyhow::{Result, ensure};
use serde::Serialize;

use crate::hidpp::device::Device;

pub const FEATURE_GKEYS: u16 = 0x8010;

pub struct GKeys<'a> {
    device: &'a Device,
    feature: u8,
}

impl<'a> GKeys<'a> {
    pub fn new(device: &'a Device) -> Result<Self> {
        let feature = device
            .require_feature(FEATURE_GKEYS)
            .map_err(anyhow::Error::new)?;
        Ok(Self { device, feature })
    }

    pub fn get_count(&self) -> Result<u8> {
        Ok(self.device.call_long(self.feature, 0, &[])?[0])
    }

    pub fn get_physical_layout(&self) -> Result<u16> {
        let response = self.device.call_long(self.feature, 1, &[])?;
        Ok(u16::from_be_bytes([response[0], response[1]]))
    }

    pub fn enable_software_control(&self, enabled: bool) -> Result<()> {
        self.device
            .call_long(self.feature, 2, &[u8::from(enabled)])?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GKeyEvent {
    pub held_mask: u32,
    pub held: Vec<u8>,
}

pub fn decode_event(payload: &[u8]) -> Result<GKeyEvent> {
    ensure!(
        payload.len() >= 2,
        "G-key event payload must contain at least 2 bytes"
    );
    let held_mask = if payload.len() >= 4 {
        u32::from_le_bytes(payload[..4].try_into().unwrap())
    } else {
        u32::from(u16::from_le_bytes(payload[..2].try_into().unwrap()))
    };
    let held = (0..32)
        .filter(|bit| held_mask & (1 << bit) != 0)
        .map(|bit| bit + 1)
        .collect();
    Ok(GKeyEvent { held_mask, held })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_le32_and_legacy_le16_masks() {
        assert_eq!(
            decode_event(&[0x05, 0, 0, 1]).unwrap(),
            GKeyEvent {
                held_mask: 0x0100_0005,
                held: vec![1, 3, 25]
            }
        );
        assert_eq!(decode_event(&[0x02, 0]).unwrap().held, [2]);
        assert!(decode_event(&[1]).is_err());
    }
}

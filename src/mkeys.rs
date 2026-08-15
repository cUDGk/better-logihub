use anyhow::{Result, ensure};
use serde::Serialize;

use crate::hidpp::device::Device;

pub const FEATURE_MKEYS: u16 = 0x8020;
pub const FEATURE_MR: u16 = 0x8030;

pub struct MKeys<'a> {
    device: &'a Device,
    feature: u8,
}

impl<'a> MKeys<'a> {
    pub fn new(device: &'a Device) -> Result<Self> {
        let feature = device
            .require_feature(FEATURE_MKEYS)
            .map_err(anyhow::Error::new)?;
        Ok(Self { device, feature })
    }

    pub fn get_count(&self) -> Result<u8> {
        Ok(self.device.call_long(self.feature, 0, &[])?[0])
    }

    pub fn set_leds(&self, mask: u8) -> Result<()> {
        ensure!(mask & !0x07 == 0, "M-key LED mask must use only bits 0..2");
        self.device.call_long(self.feature, 1, &[mask])?;
        Ok(())
    }
}

pub struct MrKey<'a> {
    device: &'a Device,
    feature: u8,
}

impl<'a> MrKey<'a> {
    pub fn new(device: &'a Device) -> Result<Self> {
        let feature = device
            .require_feature(FEATURE_MR)
            .map_err(anyhow::Error::new)?;
        Ok(Self { device, feature })
    }

    pub fn set_led(&self, enabled: bool) -> Result<()> {
        self.device
            .call_long(self.feature, 0, &[u8::from(enabled)])?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MKeyEvent {
    pub held_mask: u8,
    pub held: Vec<u8>,
}

pub fn decode_event(payload: &[u8]) -> Result<MKeyEvent> {
    let held_mask = *payload
        .first()
        .ok_or_else(|| anyhow::anyhow!("M-key event payload is empty"))?;
    let held = (0..8)
        .filter(|bit| held_mask & (1 << bit) != 0)
        .map(|bit| bit + 1)
        .collect();
    Ok(MKeyEvent { held_mask, held })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_m_key_mask() {
        assert_eq!(decode_event(&[0x05]).unwrap().held, [1, 3]);
        assert!(decode_event(&[]).is_err());
    }
}

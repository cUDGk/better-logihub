use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::hidpp::device::Device;

pub const FEATURE_SPECIAL_KEYS: u16 = 0x1B04;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CidFlags {
    pub raw: u8,
    pub mouse: bool,
    pub fkey: bool,
    pub hotkey: bool,
    pub fn_toggle: bool,
    pub reprogrammable: bool,
    pub divertable: bool,
    pub persistently_divertable: bool,
    pub virtual_control: bool,
}

impl CidFlags {
    fn decode(raw: u8) -> Self {
        Self {
            raw,
            mouse: raw & 0x01 != 0,
            fkey: raw & 0x02 != 0,
            hotkey: raw & 0x04 != 0,
            fn_toggle: raw & 0x08 != 0,
            reprogrammable: raw & 0x10 != 0,
            divertable: raw & 0x20 != 0,
            persistently_divertable: raw & 0x40 != 0,
            virtual_control: raw & 0x80 != 0,
        }
    }

    pub fn names(self) -> Vec<&'static str> {
        const FLAGS: &[(u8, &str)] = &[
            (0x01, "mouse"),
            (0x02, "fkey"),
            (0x04, "hotkey"),
            (0x08, "fn-toggle"),
            (0x10, "reprogrammable"),
            (0x20, "divertable"),
            (0x40, "persistent-divertable"),
            (0x80, "virtual"),
        ];
        FLAGS
            .iter()
            .filter(|(bit, _)| self.raw & bit != 0)
            .map(|(_, name)| *name)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AdditionalFlags {
    pub raw: u8,
    pub raw_xy: bool,
    pub force_raw_xy: bool,
    pub analytics_key_events: bool,
}

impl AdditionalFlags {
    fn decode(raw: u8) -> Self {
        Self {
            raw,
            raw_xy: raw & 0x01 != 0,
            force_raw_xy: raw & 0x02 != 0,
            analytics_key_events: raw & 0x04 != 0,
        }
    }

    pub fn names(self) -> Vec<&'static str> {
        const FLAGS: &[(u8, &str)] = &[
            (0x01, "raw-xy"),
            (0x02, "force-raw-xy"),
            (0x04, "analytics"),
        ];
        FLAGS
            .iter()
            .filter(|(bit, _)| self.raw & bit != 0)
            .map(|(_, name)| *name)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CidInfo {
    pub index: u8,
    pub cid: u16,
    pub name: String,
    pub task_id: u16,
    pub flags: CidFlags,
    pub position: u8,
    pub group: u8,
    pub group_mask: u8,
    pub additional_flags: AdditionalFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CidReporting {
    pub cid: u16,
    pub flags1_raw: u8,
    pub divert: bool,
    pub divert_valid: bool,
    pub persist: bool,
    pub persist_valid: bool,
    pub raw_xy: bool,
    pub raw_xy_valid: bool,
    pub force_raw_xy: bool,
    pub force_raw_xy_valid: bool,
    pub remap: u16,
    pub flags2_raw: u8,
    pub analytics_key_events: bool,
    pub analytics_key_events_valid: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportingUpdate {
    pub divert: Option<bool>,
    pub persist: Option<bool>,
    pub raw_xy: Option<bool>,
    pub force_raw_xy: Option<bool>,
    pub analytics_key_events: Option<bool>,
    pub remap: Option<u16>,
}

impl ReportingUpdate {
    pub fn encode_flags(self) -> (u8, u8) {
        let mut flags1 = 0;
        let mut flags2 = 0;
        set_value_valid(&mut flags1, 0x01, self.divert);
        set_value_valid(&mut flags1, 0x04, self.persist);
        set_value_valid(&mut flags1, 0x10, self.raw_xy);
        set_value_valid(&mut flags1, 0x40, self.force_raw_xy);
        set_value_valid(&mut flags2, 0x01, self.analytics_key_events);
        (flags1, flags2)
    }
}

fn set_value_valid(byte: &mut u8, value_bit: u8, value: Option<bool>) {
    if let Some(value) = value {
        if value {
            *byte |= value_bit;
        }
        *byte |= value_bit << 1;
    }
}

pub struct SpecialKeys<'a> {
    device: &'a Device,
    feature: u8,
}

impl<'a> SpecialKeys<'a> {
    pub fn new(device: &'a Device) -> Result<Self> {
        let feature = device
            .require_feature(FEATURE_SPECIAL_KEYS)
            .map_err(anyhow::Error::new)?;
        Ok(Self { device, feature })
    }

    pub fn count(&self) -> Result<u8> {
        Ok(self.device.call_long(self.feature, 0, &[])?[0])
    }

    pub fn cid_info(&self, index: u8) -> Result<CidInfo> {
        let response = self.device.call_long(self.feature, 1, &[index])?;
        parse_cid_info(index, &response)
    }

    pub fn all_cid_info(&self) -> Result<Vec<CidInfo>> {
        (0..self.count()?)
            .map(|index| self.cid_info(index))
            .collect()
    }

    pub fn reporting(&self, cid: u16) -> Result<CidReporting> {
        let response = self.device.call_long(self.feature, 2, &cid.to_be_bytes())?;
        parse_reporting(cid, &response)
    }

    pub fn set_reporting_raw(&self, cid: u16, flags1: u8, remap: u16, flags2: u8) -> Result<()> {
        let request = reporting_request(cid, flags1, remap, flags2);
        self.device.call_long(self.feature, 3, &request)?;
        Ok(())
    }

    pub fn update_reporting(&self, cid: u16, update: ReportingUpdate) -> Result<CidReporting> {
        let (flags1, flags2) = update.encode_flags();
        self.set_reporting_raw(cid, flags1, update.remap.unwrap_or(0), flags2)?;
        self.reporting(cid)
    }

    pub fn capabilities(&self) -> Result<u8> {
        Ok(self.device.call_long(self.feature, 4, &[])?[0])
    }

    pub fn reset_all(&self) -> Result<()> {
        self.device.call_long(self.feature, 5, &[])?;
        Ok(())
    }
}

fn reporting_request(cid: u16, flags1: u8, remap: u16, flags2: u8) -> [u8; 6] {
    let mut request = [0_u8; 6];
    request[..2].copy_from_slice(&cid.to_be_bytes());
    request[2] = flags1;
    request[3..5].copy_from_slice(&remap.to_be_bytes());
    request[5] = flags2;
    request
}

fn parse_cid_info(index: u8, response: &[u8]) -> Result<CidInfo> {
    ensure!(
        response.len() >= 9,
        "getCidInfo returned only {} bytes",
        response.len()
    );
    let cid = u16::from_be_bytes([response[0], response[1]]);
    Ok(CidInfo {
        index,
        cid,
        name: cid_name(cid),
        task_id: u16::from_be_bytes([response[2], response[3]]),
        flags: CidFlags::decode(response[4]),
        position: response[5],
        group: response[6],
        group_mask: response[7],
        additional_flags: AdditionalFlags::decode(response[8]),
    })
}

fn parse_reporting(cid: u16, response: &[u8]) -> Result<CidReporting> {
    ensure!(
        response.len() >= 6,
        "getCidReporting returned only {} bytes",
        response.len()
    );
    let echoed = u16::from_be_bytes([response[0], response[1]]);
    ensure!(
        echoed == cid,
        "getCidReporting echoed CID 0x{echoed:04X}, expected 0x{cid:04X}"
    );
    let flags1 = response[2];
    let flags2 = response[5];
    Ok(CidReporting {
        cid,
        flags1_raw: flags1,
        divert: flags1 & 0x01 != 0,
        divert_valid: flags1 & 0x02 != 0,
        persist: flags1 & 0x04 != 0,
        persist_valid: flags1 & 0x08 != 0,
        raw_xy: flags1 & 0x10 != 0,
        raw_xy_valid: flags1 & 0x20 != 0,
        force_raw_xy: flags1 & 0x40 != 0,
        force_raw_xy_valid: flags1 & 0x80 != 0,
        remap: u16::from_be_bytes([response[3], response[4]]),
        flags2_raw: flags2,
        analytics_key_events: flags2 & 0x01 != 0,
        analytics_key_events_valid: flags2 & 0x02 != 0,
    })
}

pub fn ensure_can_remap(source: &CidInfo, target: &CidInfo) -> Result<()> {
    ensure!(
        source.flags.reprogrammable,
        "CID 0x{:04X} is not reprogrammable",
        source.cid
    );
    if source.cid == target.cid {
        return Ok(());
    }
    ensure!(
        (1..=8).contains(&target.group) && source.group_mask & (1 << (target.group - 1)) != 0,
        "CID 0x{:04X} group mask 0x{:02X} does not allow target 0x{:04X} in group {}",
        source.cid,
        source.group_mask,
        target.cid,
        target.group
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnalyticsEntry {
    pub cid: u16,
    pub event: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpecialKeyEvent {
    DivertedButtons {
        held_cids: Vec<u16>,
    },
    DivertedRawXy {
        x: i16,
        y: i16,
    },
    AnalyticsKeyEvents {
        entries: Vec<AnalyticsEntry>,
    },
    Reserved {
        event_index: u8,
        payload: Vec<u8>,
    },
    DivertedRawWheel {
        resolution: bool,
        periods: u8,
        delta_v: i16,
    },
}

pub fn decode_event(index: u8, payload: &[u8]) -> Result<SpecialKeyEvent> {
    match index {
        0 => {
            ensure!(
                payload.len() >= 8,
                "diverted-buttons payload is shorter than 8 bytes"
            );
            let held_cids = payload[..8]
                .chunks_exact(2)
                .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
                .filter(|cid| *cid != 0)
                .collect();
            Ok(SpecialKeyEvent::DivertedButtons { held_cids })
        }
        1 => {
            ensure!(payload.len() >= 4, "raw-XY payload is shorter than 4 bytes");
            Ok(SpecialKeyEvent::DivertedRawXy {
                x: i16::from_be_bytes([payload[0], payload[1]]),
                y: i16::from_be_bytes([payload[2], payload[3]]),
            })
        }
        2 => {
            ensure!(
                payload.len() >= 15,
                "analytics payload is shorter than 15 bytes"
            );
            let entries = payload[..15]
                .chunks_exact(3)
                .map(|entry| AnalyticsEntry {
                    cid: u16::from_be_bytes([entry[0], entry[1]]),
                    event: entry[2],
                })
                .filter(|entry| entry.cid != 0)
                .collect();
            Ok(SpecialKeyEvent::AnalyticsKeyEvents { entries })
        }
        3 => Ok(SpecialKeyEvent::Reserved {
            event_index: index,
            payload: payload.to_vec(),
        }),
        4 => {
            ensure!(
                payload.len() >= 3,
                "raw-wheel payload is shorter than 3 bytes"
            );
            Ok(SpecialKeyEvent::DivertedRawWheel {
                resolution: payload[0] & 0x10 != 0,
                periods: payload[0] & 0x0F,
                delta_v: i16::from_be_bytes([payload[1], payload[2]]),
            })
        }
        _ => bail!("unknown SpecialKeys event index {index}"),
    }
}

pub fn cid_name(cid: u16) -> String {
    if (0x18F..=0x197).contains(&cid) {
        return format!("G{}", cid - 0x18E);
    }
    if (0x122..=0x13B).contains(&cid) {
        return ((b'A' + (cid - 0x122) as u8) as char).to_string();
    }
    if cid == 0x142 {
        return "0".into();
    }
    if (0x143..=0x14B).contains(&cid) {
        return (cid - 0x142).to_string();
    }
    if let Some((_, name)) = NAMED_CIDS.iter().find(|(value, _)| *value == cid) {
        return (*name).into();
    }
    format!("CID-0x{cid:04X}")
}

pub fn resolve_cid(value: &str) -> Result<u16> {
    let trimmed = value.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u16::from_str_radix(hex, 16).with_context(|| format!("invalid CID {value:?}"));
    }
    if trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return trimmed
            .parse::<u16>()
            .with_context(|| format!("invalid CID {value:?}"));
    }
    let wanted = normalize_name(trimmed);
    for cid in candidate_cids() {
        if normalize_name(&cid_name(cid)) == wanted {
            return Ok(cid);
        }
    }
    bail!("unknown CID name {value:?}; use `keys list` to inspect this device")
}

fn normalize_name(value: &str) -> String {
    value
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| byte.to_ascii_lowercase() as char)
        .collect()
}

fn candidate_cids() -> Vec<u16> {
    let mut values = NAMED_CIDS.iter().map(|(cid, _)| *cid).collect::<Vec<_>>();
    values.extend(0x122..=0x13B);
    values.extend(0x142..=0x14B);
    values.extend(0x18F..=0x197);
    values
}

const NAMED_CIDS: &[(u16, &str)] = &[
    (0x0034, "Fn"),
    (0x0050, "Left"),
    (0x0051, "Right"),
    (0x0052, "Middle"),
    (0x0053, "Back"),
    (0x0054, "Back HID"),
    (0x0056, "Forward"),
    (0x0057, "Forward HID"),
    (0x005B, "DPI Up"),
    (0x005D, "DPI Down"),
    (0x00C3, "Gesture Navigation"),
    (0x00C4, "Smart Shift"),
    (0x00D7, "Virtual Gesture"),
    (0x00E0, "DPI Switch"),
    (0x00E4, "Previous Track"),
    (0x00E5, "Play Pause"),
    (0x00E6, "Next Track"),
    (0x00E7, "Mute"),
    (0x00EB, "Right Arrow"),
    (0x00EC, "Left Arrow"),
    (0x00EF, "F2"),
    (0x00F0, "F3"),
    (0x00F1, "F4"),
    (0x00F2, "F5"),
    (0x00F3, "F6"),
    (0x00F4, "F7"),
    (0x00F5, "F8"),
    (0x00F6, "F1"),
    (0x010C, "Tab"),
    (0x010D, "Caps Lock"),
    (0x010E, "Left Shift"),
    (0x010F, "Left Ctrl"),
    (0x0114, "Right Ctrl"),
    (0x0115, "Right Shift"),
    (0x0116, "Insert"),
    (0x0117, "Delete"),
    (0x0118, "Home"),
    (0x0119, "End"),
    (0x011A, "Page Up"),
    (0x011B, "Page Down"),
    (0x014C, "Escape"),
    (0x014D, "F9"),
    (0x014E, "F10"),
    (0x014F, "F11"),
    (0x0150, "F12"),
    (0x0151, "Up Arrow"),
    (0x0152, "Down Arrow"),
    (0x0155, "Enter"),
    (0x0156, "Backspace"),
    (0x0159, "Bluetooth"),
    (0x0161, "Space"),
    (0x016B, "Num Lock"),
    (0x016C, "Numpad Divide"),
    (0x016D, "Numpad Multiply"),
    (0x016E, "Numpad Minus"),
    (0x016F, "Numpad Plus"),
    (0x0170, "Numpad Enter"),
    (0x0171, "Numpad 1"),
    (0x0172, "Numpad 2"),
    (0x0173, "Numpad 3"),
    (0x0174, "Numpad 4"),
    (0x0175, "Numpad 5"),
    (0x0176, "Numpad 6"),
    (0x0177, "Numpad 7"),
    (0x0178, "Numpad 8"),
    (0x0179, "Numpad 9"),
    (0x017A, "Numpad 0"),
    (0x017B, "Numpad Decimal"),
    (0x017D, "Kana"),
    (0x017E, "Yen"),
    (0x017F, "Henkan"),
    (0x0180, "Muhenkan"),
    (0x0183, "Lightspeed Bluetooth"),
    (0x0184, "Key Brightness Cycle"),
    (0x0185, "Game Mode"),
    (0x0186, "Roller 0 Up"),
    (0x0187, "Roller 0 Down"),
    (0x0188, "Roller 1 Up"),
    (0x0189, "Roller 1 Down"),
    (0x018A, "Lightspeed"),
    (0x018B, "Left Alt"),
    (0x018C, "Left Win"),
    (0x018D, "Right Alt"),
    (0x018E, "Right Win"),
    (0x01B2, "Backlight Off"),
    (0x01B3, "Cycle Report Rate"),
    (0x01B6, "Force Switch Scan"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_documented_info_and_reporting_flags() {
        let info = parse_cid_info(7, &[0x01, 0x8F, 0x12, 0x34, 0xF1, 5, 2, 4, 7]).unwrap();
        assert_eq!(
            (info.cid, info.task_id, info.name.as_str()),
            (0x018F, 0x1234, "G1")
        );
        assert!(info.flags.mouse && info.flags.reprogrammable && info.flags.virtual_control);
        assert!(info.additional_flags.raw_xy);
        assert!(info.additional_flags.force_raw_xy);
        assert!(info.additional_flags.analytics_key_events);

        let reporting = parse_reporting(0x018F, &[0x01, 0x8F, 0xFF, 0x01, 0x90, 0x03]).unwrap();
        assert!(
            reporting.divert && reporting.persist && reporting.raw_xy && reporting.force_raw_xy
        );
        assert!(
            reporting.divert_valid
                && reporting.persist_valid
                && reporting.raw_xy_valid
                && reporting.force_raw_xy_valid
        );
        assert!(reporting.analytics_key_events);
        assert!(reporting.analytics_key_events_valid);
        assert_eq!(reporting.remap, 0x0190);

        let every_info_flag = CidFlags::decode(0xFF);
        assert!(
            every_info_flag.mouse
                && every_info_flag.fkey
                && every_info_flag.hotkey
                && every_info_flag.fn_toggle
                && every_info_flag.reprogrammable
                && every_info_flag.divertable
                && every_info_flag.persistently_divertable
                && every_info_flag.virtual_control
        );
    }

    #[test]
    fn encodes_value_and_adjacent_valid_bits_exactly() {
        assert_eq!(
            ReportingUpdate {
                divert: Some(true),
                persist: Some(false),
                raw_xy: Some(true),
                force_raw_xy: Some(false),
                analytics_key_events: Some(true),
                remap: None,
            }
            .encode_flags(),
            (0xBB, 0x03)
        );
        assert_eq!(ReportingUpdate::default().encode_flags(), (0, 0));
        assert_eq!(
            reporting_request(0x018F, 0xBB, 0x0190, 0x03),
            [0x01, 0x8F, 0xBB, 0x01, 0x90, 0x03]
        );
    }

    #[test]
    fn decodes_all_five_special_event_indices() {
        assert_eq!(
            decode_event(0, &[0x01, 0x8F, 0x01, 0x90, 0, 0, 0, 0]).unwrap(),
            SpecialKeyEvent::DivertedButtons {
                held_cids: vec![0x018F, 0x0190]
            }
        );
        assert_eq!(
            decode_event(1, &[0xFF, 0xFE, 0x00, 0x03]).unwrap(),
            SpecialKeyEvent::DivertedRawXy { x: -2, y: 3 }
        );
        assert!(matches!(
            decode_event(2, &[0, 1, 2, 0, 2, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap(),
            SpecialKeyEvent::AnalyticsKeyEvents { entries } if entries.len() == 2
        ));
        assert!(matches!(
            decode_event(3, &[1, 2]).unwrap(),
            SpecialKeyEvent::Reserved { .. }
        ));
        assert_eq!(
            decode_event(4, &[0x13, 0xFF, 0x9C]).unwrap(),
            SpecialKeyEvent::DivertedRawWheel {
                resolution: true,
                periods: 3,
                delta_v: -100
            }
        );
    }

    #[test]
    fn resolves_public_cid_names_and_ranges() {
        assert_eq!(resolve_cid("g5").unwrap(), 0x0193);
        assert_eq!(resolve_cid("Play-Pause").unwrap(), 0x00E5);
        assert_eq!(resolve_cid("0x0050").unwrap(), 0x0050);
        assert_eq!(cid_name(0x0122), "A");
    }
}

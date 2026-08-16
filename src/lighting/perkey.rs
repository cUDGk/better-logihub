#![allow(dead_code)]

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::hidpp::device::Device;
use crate::lighting::rgb::RgbColor;

const FEATURE_PER_KEY_LIGHTING_V2: u16 = 0x8081;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ZoneScheme {
    HidUsage,
    Solaar,
}

impl FromStr for ZoneScheme {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "hidusage" | "hid_usage" | "hid-usage" => Ok(Self::HidUsage),
            "solaar" => Ok(Self::Solaar),
            _ => bail!("zone scheme must be hidusage or solaar"),
        }
    }
}

impl std::fmt::Display for ZoneScheme {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::HidUsage => "hidusage",
            Self::Solaar => "solaar",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedKey {
    pub name: String,
    pub usage: Option<u8>,
    pub zone_id: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndividualZone {
    pub zone: u8,
    pub color: RgbColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneRange {
    pub start: u8,
    pub end: u8,
    pub color: RgbColor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BatchOperation {
    Individual(Vec<IndividualZone>),
    Consecutive { first: u8, colors: Vec<RgbColor> },
    SingleValue { color: RgbColor, zones: Vec<u8> },
}

pub struct PerKeyLightingV2<'a> {
    device: &'a Device,
    feature: u8,
}

impl<'a> PerKeyLightingV2<'a> {
    pub fn new(device: &'a Device) -> Result<Self> {
        let feature = device
            .require_feature(FEATURE_PER_KEY_LIGHTING_V2)
            .map_err(anyhow::Error::new)?;
        Ok(Self { device, feature })
    }

    pub fn info(&self, info_type: u8, param: u8) -> Result<[u8; 16]> {
        self.call(0, &get_info_request(info_type, param))
    }

    pub fn set_individual(&self, zones: &[IndividualZone]) -> Result<()> {
        self.call(1, &individual_request(zones)?)?;
        Ok(())
    }

    pub fn set_consecutive(&self, first: u8, colors: &[RgbColor]) -> Result<()> {
        self.call(2, &consecutive_request(first, colors)?)?;
        Ok(())
    }

    pub fn set_delta_5bit_raw(&self, packed: [u8; 16]) -> Result<()> {
        self.call(3, &delta_request(packed))?;
        Ok(())
    }

    pub fn set_delta_4bit_raw(&self, packed: [u8; 16]) -> Result<()> {
        self.call(4, &delta_request(packed))?;
        Ok(())
    }

    pub fn set_ranges(&self, ranges: &[ZoneRange]) -> Result<()> {
        self.call(5, &range_request(ranges)?)?;
        Ok(())
    }

    pub fn set_single_value(&self, color: RgbColor, zones: &[u8]) -> Result<()> {
        self.call(6, &single_value_request(color, zones)?)?;
        Ok(())
    }

    pub fn frame_end(
        &self,
        persistent: bool,
        current_frame: u16,
        frames_until_change: u16,
    ) -> Result<()> {
        self.call(
            7,
            &frame_end_request(persistent, current_frame, frames_until_change),
        )?;
        Ok(())
    }

    pub fn write_colors(&self, colors: &[(u8, RgbColor)], persistent: bool) -> Result<usize> {
        let operations = plan_operations(colors)?;
        for operation in &operations {
            match operation {
                BatchOperation::Individual(zones) => self.set_individual(zones)?,
                BatchOperation::Consecutive { first, colors } => {
                    self.set_consecutive(*first, colors)?
                }
                BatchOperation::SingleValue { color, zones } => {
                    self.set_single_value(*color, zones)?
                }
            }
        }
        self.frame_end(persistent, 0, 0)?;
        Ok(operations.len())
    }

    fn call(&self, function: u8, params: &[u8]) -> Result<[u8; 16]> {
        self.device
            .call_long(self.feature, function, params)
            .map_err(anyhow::Error::new)
    }
}

pub fn resolve_key(name: &str, scheme: ZoneScheme) -> Result<ResolvedKey> {
    let lower = name
        .trim()
        .to_ascii_lowercase()
        .replace(['-', '_', ' '], "");
    if let Some(number) = lower
        .strip_prefix('g')
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|number| (1..=5).contains(number))
    {
        return Ok(ResolvedKey {
            name: format!("g{number}"),
            usage: None,
            zone_id: 179 + number,
        });
    }
    if matches!(lower.as_str(), "logo" | "branding") {
        return Ok(ResolvedKey {
            name: "logo".into(),
            usage: None,
            zone_id: 210,
        });
    }

    let usage = key_usage(&lower).with_context(|| format!("unknown per-key name {name:?}"))?;
    let zone_id = match scheme {
        ZoneScheme::HidUsage => usage,
        ZoneScheme::Solaar => solaar_zone(usage)
            .with_context(|| format!("key {name:?} has no Solaar flat-table zone"))?,
    };
    Ok(ResolvedKey {
        name: lower,
        usage: Some(usage),
        zone_id,
    })
}

pub fn zones_from_usages<I>(usages: I, scheme: ZoneScheme) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = u8>,
{
    // Bulk fill/clear: usages outside the Solaar table (e.g. 0x87 international keys)
    // are skipped rather than failing the whole frame; `perkey set` still errors per key.
    let mut zones = usages
        .into_iter()
        .filter_map(|usage| match scheme {
            ZoneScheme::HidUsage => Some(usage),
            ZoneScheme::Solaar => solaar_zone(usage),
        })
        .collect::<Vec<_>>();
    zones.sort_unstable();
    zones.dedup();
    Ok(zones)
}

pub fn probe_zones() -> [IndividualZone; 4] {
    [
        IndividualZone {
            zone: 0x04,
            color: RgbColor { r: 255, g: 0, b: 0 },
        },
        IndividualZone {
            zone: 0x05,
            color: RgbColor { r: 0, g: 255, b: 0 },
        },
        IndividualZone {
            zone: 0x01,
            color: RgbColor { r: 0, g: 0, b: 255 },
        },
        IndividualZone {
            zone: 0x02,
            color: RgbColor {
                r: 255,
                g: 255,
                b: 0,
            },
        },
    ]
}

fn key_usage(key: &str) -> Option<u8> {
    if key.len() == 1 {
        let byte = key.as_bytes()[0];
        return match byte {
            b'a'..=b'z' => Some(0x04 + byte - b'a'),
            b'1'..=b'9' => Some(0x1E + byte - b'1'),
            b'0' => Some(0x27),
            _ => None,
        };
    }
    if let Some(number) = key
        .strip_prefix('f')
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|number| (1..=12).contains(number))
    {
        return Some(0x3A + number - 1);
    }
    if let Some(value) = key.strip_prefix("usage0x") {
        return u8::from_str_radix(value, 16).ok();
    }
    Some(match key {
        "enter" | "return" => 0x28,
        "esc" | "escape" => 0x29,
        "backspace" => 0x2A,
        "tab" => 0x2B,
        "space" => 0x2C,
        "minus" => 0x2D,
        "equal" => 0x2E,
        "leftbracket" => 0x2F,
        "rightbracket" => 0x30,
        "backslash" => 0x31,
        "semicolon" => 0x33,
        "quote" => 0x34,
        "grave" | "backtick" => 0x35,
        "comma" => 0x36,
        "period" | "dot" => 0x37,
        "slash" => 0x38,
        "capslock" => 0x39,
        "printscreen" => 0x46,
        "scrolllock" => 0x47,
        "pause" => 0x48,
        "insert" => 0x49,
        "home" => 0x4A,
        "pageup" | "pgup" => 0x4B,
        "delete" | "del" => 0x4C,
        "end" => 0x4D,
        "pagedown" | "pgdn" => 0x4E,
        "right" => 0x4F,
        "left" => 0x50,
        "down" => 0x51,
        "up" => 0x52,
        "numlock" => 0x53,
        "leftctrl" | "lctrl" => 0xE0,
        "leftshift" | "lshift" => 0xE1,
        "leftalt" | "lalt" => 0xE2,
        "leftwin" | "lwin" | "leftgui" => 0xE3,
        "rightctrl" | "rctrl" => 0xE4,
        "rightshift" | "rshift" => 0xE5,
        "rightalt" | "ralt" | "altgr" => 0xE6,
        "rightwin" | "rwin" | "rightgui" => 0xE7,
        _ => return None,
    })
}

fn solaar_zone(usage: u8) -> Option<u8> {
    match usage {
        0x04..=0x65 => Some(usage - 3),
        0xE0..=0xE7 => Some(usage - 0x78),
        _ => None,
    }
}

fn plan_operations(colors: &[(u8, RgbColor)]) -> Result<Vec<BatchOperation>> {
    let input_len = colors.len();
    let mut colors = BTreeMap::from_iter(colors.iter().copied());
    ensure!(colors.len() == input_len, "duplicate per-key zone id");
    if colors.is_empty() {
        return Ok(Vec::new());
    }
    let entries = colors
        .pop_first()
        .into_iter()
        .chain(colors)
        .collect::<Vec<_>>();
    if entries.iter().all(|(_, color)| *color == entries[0].1) {
        return Ok(entries
            .chunks(13)
            .map(|chunk| BatchOperation::SingleValue {
                color: chunk[0].1,
                zones: chunk.iter().map(|(zone, _)| *zone).collect(),
            })
            .collect());
    }

    let mut operations = Vec::new();
    let mut individual = Vec::new();
    let mut index = 0;
    while index < entries.len() {
        let mut end = index + 1;
        while end < entries.len()
            && end - index < 5
            && entries[end].0 == entries[end - 1].0.saturating_add(1)
        {
            end += 1;
        }
        if end - index >= 2 {
            flush_individual(&mut operations, &mut individual);
            operations.push(BatchOperation::Consecutive {
                first: entries[index].0,
                colors: entries[index..end]
                    .iter()
                    .map(|(_, color)| *color)
                    .collect(),
            });
            index = end;
        } else {
            individual.push(IndividualZone {
                zone: entries[index].0,
                color: entries[index].1,
            });
            index += 1;
            if individual.len() == 4 {
                flush_individual(&mut operations, &mut individual);
            }
        }
    }
    flush_individual(&mut operations, &mut individual);
    Ok(operations)
}

fn flush_individual(operations: &mut Vec<BatchOperation>, individual: &mut Vec<IndividualZone>) {
    if !individual.is_empty() {
        operations.push(BatchOperation::Individual(std::mem::take(individual)));
    }
}

fn get_info_request(info_type: u8, param: u8) -> [u8; 2] {
    [info_type, param]
}

fn individual_request(zones: &[IndividualZone]) -> Result<[u8; 16]> {
    ensure!(
        !zones.is_empty() && zones.len() <= 4,
        "individual write requires 1..=4 zones"
    );
    let mut request = [0_u8; 16];
    for (index, zone) in zones.iter().enumerate() {
        request[index * 4..index * 4 + 4].copy_from_slice(&[
            zone.zone,
            zone.color.r,
            zone.color.g,
            zone.color.b,
        ]);
    }
    Ok(request)
}

fn consecutive_request(first: u8, colors: &[RgbColor]) -> Result<[u8; 16]> {
    ensure!(
        !colors.is_empty() && colors.len() <= 5,
        "consecutive write requires 1..=5 colors"
    );
    ensure!(
        usize::from(first) + colors.len() - 1 <= usize::from(u8::MAX),
        "consecutive zone range overflows u8"
    );
    let mut request = [0_u8; 16];
    request[0] = first;
    for (index, color) in colors.iter().enumerate() {
        request[1 + index * 3..4 + index * 3].copy_from_slice(&[color.r, color.g, color.b]);
    }
    Ok(request)
}

fn range_request(ranges: &[ZoneRange]) -> Result<[u8; 16]> {
    ensure!(
        !ranges.is_empty() && ranges.len() <= 3,
        "range write requires 1..=3 ranges"
    );
    let mut request = [0_u8; 16];
    for (index, range) in ranges.iter().enumerate() {
        ensure!(
            range.start <= range.end,
            "zone range start must not exceed end"
        );
        request[index * 5..index * 5 + 5].copy_from_slice(&[
            range.start,
            range.end,
            range.color.r,
            range.color.g,
            range.color.b,
        ]);
    }
    Ok(request)
}

fn delta_request(packed: [u8; 16]) -> [u8; 16] {
    packed
}

fn single_value_request(color: RgbColor, zones: &[u8]) -> Result<[u8; 16]> {
    ensure!(
        !zones.is_empty() && zones.len() <= 13,
        "single-value write requires 1..=13 zones"
    );
    let mut request = [0_u8; 16];
    request[..3].copy_from_slice(&[color.r, color.g, color.b]);
    request[3..3 + zones.len()].copy_from_slice(zones);
    Ok(request)
}

fn frame_end_request(persistent: bool, current_frame: u16, frames_until_change: u16) -> [u8; 5] {
    let mut request = [0_u8; 5];
    request[0] = u8::from(persistent);
    request[1..3].copy_from_slice(&current_frame.to_be_bytes());
    request[3..5].copy_from_slice(&frames_until_change.to_be_bytes());
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: RgbColor = RgbColor { r: 255, g: 0, b: 0 };
    const GREEN: RgbColor = RgbColor { r: 0, g: 255, b: 0 };

    #[test]
    fn encodes_all_eight_request_layouts_from_the_spec_table() {
        assert_eq!(get_info_request(2, 7), [2, 7]);
        assert_eq!(
            individual_request(&[
                IndividualZone {
                    zone: 4,
                    color: RED
                },
                IndividualZone {
                    zone: 5,
                    color: GREEN
                },
            ])
            .unwrap(),
            [4, 255, 0, 0, 5, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            consecutive_request(4, &[RED, GREEN]).unwrap(),
            [4, 255, 0, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        let raw = std::array::from_fn(|index| index as u8);
        assert_eq!(
            delta_request(raw),
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(delta_request(raw), raw);
        assert_eq!(
            range_request(&[ZoneRange {
                start: 4,
                end: 29,
                color: RED,
            }])
            .unwrap(),
            [4, 29, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            single_value_request(RED, &[4, 5, 6]).unwrap(),
            [255, 0, 0, 4, 5, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            frame_end_request(true, 0x1234, 0xABCD),
            [1, 0x12, 0x34, 0xAB, 0xCD]
        );
    }

    #[test]
    fn resolves_both_candidate_zone_numberings_and_special_zones() {
        assert_eq!(resolve_key("A", ZoneScheme::HidUsage).unwrap().zone_id, 4);
        assert_eq!(resolve_key("a", ZoneScheme::Solaar).unwrap().zone_id, 1);
        assert_eq!(
            resolve_key("F1", ZoneScheme::HidUsage).unwrap().zone_id,
            0x3A
        );
        assert_eq!(
            resolve_key("left-ctrl", ZoneScheme::Solaar)
                .unwrap()
                .zone_id,
            104
        );
        assert_eq!(
            resolve_key("g5", ZoneScheme::HidUsage).unwrap().zone_id,
            184
        );
        assert_eq!(
            resolve_key("logo", ZoneScheme::Solaar).unwrap().zone_id,
            210
        );
    }

    #[test]
    fn batches_uniform_and_consecutive_frames_at_protocol_maxima() {
        let uniform = (1..=14).map(|zone| (zone, RED)).collect::<Vec<_>>();
        let operations = plan_operations(&uniform).unwrap();
        assert_eq!(operations.len(), 2);
        assert!(
            matches!(&operations[0], BatchOperation::SingleValue { zones, .. } if zones.len() == 13)
        );

        let mixed = vec![(4, RED), (5, GREEN), (20, RED)];
        let operations = plan_operations(&mixed).unwrap();
        assert!(
            matches!(&operations[0], BatchOperation::Consecutive { first: 4, colors } if colors.len() == 2)
        );
        assert!(matches!(&operations[1], BatchOperation::Individual(zones) if zones.len() == 1));
    }

    #[test]
    fn probe_uses_distinct_colors_for_both_hypotheses() {
        let probe = probe_zones();
        assert_eq!((probe[0].zone, probe[1].zone), (4, 5));
        assert_eq!((probe[2].zone, probe[3].zone), (1, 2));
        assert_ne!(probe[0].color, probe[2].color);
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::hidpp::device::Device;
use crate::lighting::rgb::{Effect, EffectOptions, encode_effect, raw_effect_name};

const FEATURE_ONBOARD_PROFILES: u16 = 0x8100;
const BUTTONS_OFFSET: usize = 32;
const GSHIFT_BUTTONS_OFFSET: usize = 96;
const BUTTON_COUNT: usize = 16;
const PROFILE_NAME_OFFSET: usize = 0xA0;
const PROFILE_NAME_UNITS: usize = 24;
const LED_SLOTS_OFFSET: usize = 0xD0;
const LED_SLOT_SIZE: usize = 11;
const LED_SLOT_COUNT: usize = 4;
const MIN_PROFILE_SECTOR_SIZE: usize = GSHIFT_BUTTONS_OFFSET + BUTTON_COUNT * 4 + 2;
const DUMP_MAGIC: &[u8; 8] = b"BLHOB001";
const EXPORT_VERSION: u32 = 1;
type PackedMacros = (Vec<(u16, Vec<u8>)>, Vec<(u16, u16)>);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Description {
    pub raw: [u8; 16],
    pub memory_model_id: u8,
    pub profile_format_id: u8,
    pub macro_format_id: u8,
    pub profile_count: u8,
    pub profile_count_oob: u8,
    pub button_count: u8,
    pub sector_count: u8,
    pub sector_size: u16,
    pub mechanical_layout: u8,
    pub various_info: u8,
}

impl Description {
    fn parse(raw: [u8; 16]) -> Result<Self> {
        let description = Self {
            raw,
            memory_model_id: raw[0],
            profile_format_id: raw[1],
            macro_format_id: raw[2],
            profile_count: raw[3],
            profile_count_oob: raw[4],
            button_count: raw[5],
            sector_count: raw[6],
            sector_size: u16::from_be_bytes([raw[7], raw[8]]),
            mechanical_layout: raw[9],
            various_info: raw[10],
        };
        description.validate()?;
        Ok(description)
    }

    fn parse_response(response: &[u8]) -> Result<Self> {
        ensure!(
            response.len() >= 10,
            "getDescription returned only {} bytes",
            response.len()
        );
        let mut raw = [0_u8; 16];
        let count = response.len().min(raw.len());
        raw[..count].copy_from_slice(&response[..count]);
        Self::parse(raw)
    }

    fn validate(&self) -> Result<()> {
        if self.memory_model_id != 0x01 {
            eprintln!(
                "warning: unrecognized onboard memory model 0x{:02X}",
                self.memory_model_id
            );
        }
        ensure!(
            matches!(self.profile_format_id, 0x01..=0x05),
            "unsupported profile format 0x{:02X}",
            self.profile_format_id
        );
        if self.macro_format_id != 0x01 {
            eprintln!(
                "warning: unrecognized onboard macro format 0x{:02X}",
                self.macro_format_id
            );
        }
        let size = usize::from(self.sector_size);
        ensure!(
            (MIN_PROFILE_SECTOR_SIZE..=4096).contains(&size),
            "unsafe or unsupported sector size {} (description raw: {:02X?})",
            self.sector_size,
            self.raw
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub index: usize,
    pub sector: u16,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    Mouse(u8),
    Key {
        modifiers: u8,
        usage: u8,
    },
    Consumer(u16),
    Macro {
        sector: u16,
        offset: u16,
    },
    Special(u8),
    /// 0x90 0x0D 0xFF <n>: switch to onboard profile n (libratbag ENABLE_PROFILE); G913 M-keys ship with this.
    EnableProfile(u8),
    Disabled,
    Other([u8; 4]),
}

impl Binding {
    pub fn encode(&self) -> Result<[u8; 4]> {
        match self {
            Self::Mouse(button) => {
                ensure!((1..=16).contains(button), "mouse button must be 1..=16");
                let flags = 1_u16 << (button - 1);
                let [high, low] = flags.to_be_bytes();
                Ok([0x80, 0x01, high, low])
            }
            Self::Key { modifiers, usage } => Ok([0x80, 0x02, *modifiers, *usage]),
            Self::Consumer(usage) => {
                let [high, low] = usage.to_be_bytes();
                Ok([0x80, 0x03, high, low])
            }
            Self::Macro { sector, offset } => {
                let [sector_high, sector_low] = sector.to_be_bytes();
                let [offset_high, offset_low] = offset.to_be_bytes();
                Ok([sector_high, sector_low, offset_high, offset_low])
            }
            Self::Special(code) => Ok([0x90, *code, 0, 0]),
            Self::EnableProfile(profile) => Ok([0x90, 0x0D, 0xFF, *profile]),
            Self::Disabled => Ok([0xFF, 0, 0, 0]),
            Self::Other(raw) => Ok(*raw),
        }
    }

    pub fn decode(raw: [u8; 4]) -> Self {
        match (raw[0], raw[1]) {
            (0x80, 0x00) => Self::Disabled,
            (0x80, 0x01) => {
                let flags = u16::from_be_bytes([raw[2], raw[3]]);
                if flags.is_power_of_two() {
                    Self::Mouse(flags.trailing_zeros() as u8 + 1)
                } else {
                    Self::Other(raw)
                }
            }
            (0x80, 0x02) => Self::Key {
                modifiers: raw[2],
                usage: raw[3],
            },
            (0x80, 0x03) => Self::Consumer(u16::from_be_bytes([raw[2], raw[3]])),
            (0x00, _) => Self::Macro {
                sector: u16::from_be_bytes([raw[0], raw[1]]),
                offset: u16::from_be_bytes([raw[2], raw[3]]),
            },
            (0x90, 0x0D) if raw[2] == 0xFF => Self::EnableProfile(raw[3]),
            // specials carry no operand; anything else must round-trip as raw bytes
            (0x90, _) if raw[2] == 0 && raw[3] == 0 => Self::Special(raw[1]),
            (0xFF, _) => Self::Disabled,
            _ => Self::Other(raw),
        }
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mouse(button) => write!(f, "mouse:{}", mouse_button_name(*button)),
            Self::Key { modifiers, usage } => {
                write!(f, "key:{}", format_key_combo(*modifiers, *usage))
            }
            Self::Consumer(usage) => write!(f, "key:{}", consumer_name(*usage)),
            Self::Macro { sector, offset } => {
                write!(f, "macro:0x{sector:04X}:0x{offset:04X}")
            }
            Self::Special(code) => write!(f, "{}", special_name(*code)),
            Self::EnableProfile(profile) => write!(f, "enable-profile:{profile}"),
            Self::Disabled => write!(f, "disabled"),
            Self::Other(raw) => write!(
                f,
                "raw:{:02X}{:02X}{:02X}{:02X}",
                raw[0], raw[1], raw[2], raw[3]
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ButtonRow {
    pub button: String,
    pub gshift: bool,
    pub binding: String,
    pub raw: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationMethod {
    GetCrc,
    ReadBack,
}

impl fmt::Display for VerificationMethod {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::GetCrc => "GetCRC",
            Self::ReadBack => "read-back",
        })
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CrcResponse {
    pub raw: [u8; 16],
    pub crc: u16,
}

#[derive(Debug, Clone)]
pub struct SectorDump {
    pub description: Description,
    pub sectors: Vec<(u16, Vec<u8>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Macro {
    pub steps: Vec<MacroStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
pub enum MacroStep {
    KeyPress { key_press: String },
    KeyRelease { key_release: String },
    Key { key: String },
    Delay { delay_ms: u16 },
    Consumer { consumer: String },
    Text { text: String },
    WaitForRelease { wait_for_release: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LedSlot {
    pub slot: usize,
    pub effect: String,
    pub raw_id: u8,
    pub parameters_hex: String,
    pub raw_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DpiTable {
    pub levels: Vec<u16>,
    pub default_index: u8,
    pub shift_index: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportBinding {
    pub number: usize,
    pub control: String,
    pub binding: String,
    pub raw_hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#macro: Option<Macro>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportProfile {
    pub index: usize,
    pub sector: u16,
    pub enabled: bool,
    pub name: Option<String>,
    pub rate_hz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpi: Option<DpiTable>,
    pub bindings: Vec<ExportBinding>,
    pub gshift_bindings: Vec<ExportBinding>,
    pub led_slots: Vec<LedSlot>,
    pub raw_sector_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportDirectory {
    pub entries: Vec<DirectoryEntry>,
    pub raw_sector_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawSector {
    pub sector: u16,
    pub raw_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OnboardExport {
    pub version: u32,
    pub device_type: String,
    pub description: Description,
    pub directory: ExportDirectory,
    pub profiles: Vec<ExportProfile>,
    pub macro_sectors: Vec<RawSector>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectorDiff {
    pub sector: u16,
    pub current_crc: u16,
    pub replacement_crc: u16,
    pub current: Vec<u8>,
    pub replacement: Vec<u8>,
}

pub struct Onboard<'a> {
    device: &'a Device,
    feature: u8,
}

impl<'a> Onboard<'a> {
    pub fn new(device: &'a Device) -> Result<Self> {
        let feature = device
            .require_feature(FEATURE_ONBOARD_PROFILES)
            .map_err(anyhow::Error::new)?;
        Ok(Self { device, feature })
    }

    pub fn description(&self) -> Result<Description> {
        let response = self
            .device
            .call_short(self.feature, 0, &[])
            .map_err(anyhow::Error::new)?;
        Description::parse_response(&response)
    }

    pub fn mode(&self) -> Result<u8> {
        let response = self
            .device
            .call_short(self.feature, 2, &[])
            .map_err(anyhow::Error::new)?;
        response
            .first()
            .copied()
            .context("getOnboardMode returned no data")
    }

    pub fn set_mode(&self, mode: u8) -> Result<()> {
        ensure!(matches!(mode, 0x01 | 0x02), "mode must be onboard or host");
        self.device
            .call_short(self.feature, 1, &[mode])
            .map_err(anyhow::Error::new)?;
        ensure!(
            self.mode()? == mode,
            "mode read-back did not match requested mode"
        );
        Ok(())
    }

    // libratbag: get fn 0xB (params[0]=index), set fn 0xC (params[0]=index, 0-based, <=4)
    pub fn current_dpi_index(&self) -> Result<u8> {
        let response = self
            .device
            .call_short(self.feature, 0x0B, &[])
            .map_err(anyhow::Error::new)?;
        response
            .first()
            .copied()
            .context("getCurrentDpiIndex returned no data")
    }

    pub fn set_current_dpi_index(&self, index: u8) -> Result<()> {
        ensure!(index <= 4, "DPI index must be 0..=4");
        self.device
            .call_short(self.feature, 0x0C, &[index])
            .map_err(anyhow::Error::new)?;
        ensure!(
            self.current_dpi_index()? == index,
            "DPI index read-back did not match"
        );
        Ok(())
    }

    pub fn current_profile(&self) -> Result<u16> {
        let response = self
            .device
            .call_short(self.feature, 0x04, &[])
            .map_err(anyhow::Error::new)?;
        parse_current_profile(&response)
    }

    pub fn get_crc(&self, sector: u16) -> Result<CrcResponse> {
        let raw = self
            .device
            .call_long(self.feature, 10, &crc_request(sector))
            .map_err(anyhow::Error::new)?;
        Ok(parse_crc_response(raw))
    }

    pub fn execute_macro(&self, sector: u16, offset: u16) -> Result<()> {
        self.device
            .call_long(self.feature, 9, &execute_macro_request(sector, offset))
            .map_err(anyhow::Error::new)?;
        Ok(())
    }

    pub fn read_sector(&self, sector: u16, size: u16) -> Result<Vec<u8>> {
        let size = usize::from(size);
        ensure!(size >= 16, "sector size is smaller than one transfer");
        let mut data = vec![0_u8; size];
        let mut offset = 0_usize;
        while offset < size {
            let read_offset = if size - offset < 16 {
                size - 16
            } else {
                offset
            };
            let mut params = [0_u8; 4];
            params[..2].copy_from_slice(&sector.to_be_bytes());
            params[2..].copy_from_slice(&(read_offset as u16).to_be_bytes());
            let response = self
                .device
                .call_long(self.feature, 5, &params)
                .map_err(anyhow::Error::new)?;
            data[read_offset..read_offset + 16].copy_from_slice(&response);
            offset = read_offset + 16;
        }
        Ok(data)
    }

    pub fn write_sector_verified(
        &self,
        sector: u16,
        expected_original: &[u8],
        replacement: &[u8],
        allow_directory: bool,
    ) -> Result<VerificationMethod> {
        ensure!(
            sector != 0 || allow_directory,
            "sector 0 may only be written by restore"
        );
        ensure!(
            expected_original.len() == replacement.len(),
            "sector length changed"
        );
        ensure!(
            sector_intact(expected_original),
            "current sector CRC is invalid; refusing to write"
        );
        ensure!(
            sector_crc_valid(replacement),
            "replacement sector CRC is invalid; refusing to write"
        );
        let size = u16::try_from(replacement.len()).context("sector is too large")?;
        let current = self.read_sector(sector, size)?;
        ensure!(
            current == expected_original,
            "sector changed after it was read; refusing to write"
        );

        let mut start = [0_u8; 6];
        start[..2].copy_from_slice(&sector.to_be_bytes());
        start[2..4].copy_from_slice(&0_u16.to_be_bytes());
        start[4..].copy_from_slice(&size.to_be_bytes());
        self.device
            .call_long(self.feature, 6, &start)
            .map_err(anyhow::Error::new)?;
        for chunk in write_chunks(replacement) {
            if let Err(error) = self.device.call_long(self.feature, 7, &chunk) {
                let _ = self.device.call_short(self.feature, 8, &[]);
                return Err(anyhow::Error::new(error));
            }
        }
        self.device
            .call_short(self.feature, 8, &[])
            .map_err(anyhow::Error::new)?;

        let expected_crc = u16::from_be_bytes([
            replacement[replacement.len() - 2],
            replacement[replacement.len() - 1],
        ]);
        if self
            .get_crc(sector)
            .is_ok_and(|response| crc_response_is_unambiguous(&response, expected_crc))
        {
            return Ok(VerificationMethod::GetCrc);
        }

        let verified = self.read_sector(sector, size)?;
        ensure!(
            verified == replacement,
            "sector read-back differs from written data"
        );
        ensure!(
            sector_crc_valid(&verified),
            "sector read-back CRC is invalid"
        );
        Ok(VerificationMethod::ReadBack)
    }

    pub fn directory(&self, description: &Description) -> Result<(Vec<u8>, Vec<DirectoryEntry>)> {
        let data = self.read_sector(0, description.sector_size)?;
        let entries = parse_directory(&data, description.profile_count)?;
        Ok((data, entries))
    }

    pub fn dump(&self) -> Result<SectorDump> {
        let description = self.description()?;
        let (directory, _) = self.directory(&description)?;
        let mut sectors = vec![(0, directory)];
        for sector in 1..u16::from(description.sector_count) {
            let data = self.read_sector(sector, description.sector_size)?;
            // Never-written sectors (factory 0xFF fill, e.g. unused macro sectors on a G913)
            // carry no CRC; keep them as-is instead of rejecting the whole dump.
            ensure!(
                sector_intact(&data),
                "sector 0x{sector:04X} has an invalid CRC"
            );
            sectors.push((sector, data));
        }
        Ok(SectorDump {
            description,
            sectors,
        })
    }
}

/// A sector is acceptable in dumps/exports when its CRC verifies OR it was never written
/// (factory 0xFF fill; unused macro sectors on a G913 look like this).
pub fn sector_intact(data: &[u8]) -> bool {
    data.iter().all(|byte| *byte == 0xFF) || sector_crc_valid(data)
}

pub fn parse_directory(data: &[u8], profile_count: u8) -> Result<Vec<DirectoryEntry>> {
    ensure!(sector_crc_valid(data), "profile directory CRC is invalid");
    let count = usize::from(profile_count);
    let terminator_offset = count * 4;
    ensure!(
        terminator_offset + 2 <= data.len() - 2,
        "profile directory is truncated"
    );
    let mut entries = Vec::new();
    for index in 0..count {
        let offset = index * 4;
        ensure!(
            data[offset] == 0 && (1..=profile_count).contains(&data[offset + 1]),
            "profile directory entry {} has invalid sector 0x{:02X}{:02X}",
            index + 1,
            data[offset],
            data[offset + 1]
        );
        let sector = u16::from(data[offset + 1]);
        entries.push(DirectoryEntry {
            index: index + 1,
            sector,
            enabled: data[offset + 2] != 0,
        });
    }
    ensure!(
        data[terminator_offset..terminator_offset + 2] == [0xFF, 0xFF],
        "profile directory has no 0xFFFF terminator at index {count}"
    );
    Ok(entries)
}

fn parse_current_profile(response: &[u8]) -> Result<u16> {
    ensure!(
        response.len() >= 2,
        "getCurrentProfile returned only {} bytes",
        response.len()
    );
    Ok(u16::from_be_bytes([response[0], response[1]]))
}

fn parse_crc_response(raw: [u8; 16]) -> CrcResponse {
    CrcResponse {
        crc: u16::from_be_bytes([raw[0], raw[1]]),
        raw,
    }
}

// GetCRC (fn 10) returns the CRCs of 8 consecutive sectors starting at the requested one,
// as 8 x BE16 (verified on a G913: [0x49B1 0xD6A2 0xC79C 0xB92A ...] = sectors 1,2,3,4...).
// So the first BE16 is authoritative; the tail is other sectors' CRCs, not padding.
fn crc_response_is_unambiguous(response: &CrcResponse, expected: u16) -> bool {
    response.crc == expected
}

fn crc_request(sector: u16) -> [u8; 2] {
    sector.to_be_bytes()
}

fn execute_macro_request(sector: u16, offset: u16) -> [u8; 4] {
    let mut request = [0_u8; 4];
    request[..2].copy_from_slice(&sector.to_be_bytes());
    request[2..].copy_from_slice(&offset.to_be_bytes());
    request
}

pub fn first_enabled_sector(entries: &[DirectoryEntry]) -> Result<u16> {
    entries
        .iter()
        .find(|entry| entry.enabled)
        .map(|entry| entry.sector)
        .context("profile directory has no enabled profile")
}

pub fn button_rows(sector: &[u8], button_count: u8, keyboard: bool) -> Result<Vec<ButtonRow>> {
    validate_profile_sector(sector)?;
    let count = BUTTON_COUNT.min(usize::from(button_count));
    let mut rows = Vec::with_capacity(count * 2);
    for (gshift, base) in [(false, BUTTONS_OFFSET), (true, GSHIFT_BUTTONS_OFFSET)] {
        for index in 0..count {
            let offset = base + index * 4;
            let raw: [u8; 4] = sector[offset..offset + 4].try_into().unwrap();
            rows.push(ButtonRow {
                button: if keyboard {
                    format!("g{}", index + 1)
                } else {
                    format!("button {}", index + 1)
                },
                gshift,
                binding: Binding::decode(raw).to_string(),
                raw: raw.iter().map(|byte| format!("{byte:02X}")).collect(),
            });
        }
    }
    Ok(rows)
}

pub fn set_button(
    sector: &mut [u8],
    number: usize,
    gshift: bool,
    binding: &Binding,
    button_count: u8,
) -> Result<()> {
    validate_profile_sector(sector)?;
    let count = BUTTON_COUNT.min(usize::from(button_count));
    ensure!(
        (1..=count).contains(&number),
        "button number must be 1..={count}"
    );
    let base = if gshift {
        GSHIFT_BUTTONS_OFFSET
    } else {
        BUTTONS_OFFSET
    };
    let offset = base + (number - 1) * 4;
    sector[offset..offset + 4].copy_from_slice(&binding.encode()?);
    update_sector_crc(sector)
}

pub fn set_dpi(
    sector: &mut [u8],
    levels: &[u16],
    default_index: usize,
    shift: Option<u16>,
) -> Result<()> {
    validate_profile_sector(sector)?;
    ensure!(
        !levels.is_empty() && levels.len() <= 5,
        "DPI requires 1 to 5 levels"
    );
    ensure!(
        default_index < levels.len(),
        "default DPI index must be 0..{}",
        levels.len() - 1
    );
    let shift_index = match shift {
        Some(dpi) => levels
            .iter()
            .position(|level| *level == dpi)
            .with_context(|| format!("shift DPI {dpi} is not present in levels"))?
            as u8,
        None => 0xFF,
    };
    sector[1] = default_index as u8;
    sector[2] = shift_index;
    for index in 0..5 {
        let dpi = levels.get(index).copied().unwrap_or(0);
        sector[3 + index * 2..5 + index * 2].copy_from_slice(&dpi.to_le_bytes());
    }
    update_sector_crc(sector)
}

pub fn set_rate(sector: &mut [u8], hz: u32, profile_format_id: u8) -> Result<()> {
    validate_profile_sector(sector)?;
    ensure!(
        matches!(profile_format_id, 1..=5),
        "report-rate editing requires onboard layout A"
    );
    ensure!(
        hz != 0 && 1000 % hz == 0,
        "onboard report rate must divide 1000 Hz exactly"
    );
    let interval = 1000 / hz;
    ensure!(
        (1..=u32::from(u8::MAX)).contains(&interval),
        "report rate is outside the onboard format range"
    );
    sector[0] = interval as u8;
    update_sector_crc(sector)
}

pub fn profile_name(sector: &[u8]) -> Result<Option<String>> {
    validate_profile_sector(sector)?;
    ensure!(
        sector.len() >= PROFILE_NAME_OFFSET + PROFILE_NAME_UNITS * 2 + 2,
        "profile sector is too short for a layout-A name"
    );
    let bytes = &sector[PROFILE_NAME_OFFSET..PROFILE_NAME_OFFSET + PROFILE_NAME_UNITS * 2];
    if bytes.iter().all(|byte| *byte == 0xFF) {
        return Ok(None);
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| !matches!(unit, 0 | 0xFFFF))
        .collect::<Vec<_>>();
    String::from_utf16(&units)
        .map(Some)
        .context("profile name is not valid UTF-16LE")
}

pub fn set_profile_name(sector: &mut [u8], name: &str) -> Result<()> {
    validate_profile_sector(sector)?;
    ensure!(
        sector.len() >= PROFILE_NAME_OFFSET + PROFILE_NAME_UNITS * 2 + 2,
        "profile sector is too short for a layout-A name"
    );
    let units = name.encode_utf16().collect::<Vec<_>>();
    ensure!(
        units.len() <= PROFILE_NAME_UNITS,
        "profile name must be at most {PROFILE_NAME_UNITS} UTF-16 code units"
    );
    let bytes = &mut sector[PROFILE_NAME_OFFSET..PROFILE_NAME_OFFSET + PROFILE_NAME_UNITS * 2];
    bytes.fill(0xFF);
    for (index, unit) in units.into_iter().enumerate() {
        bytes[index * 2..index * 2 + 2].copy_from_slice(&unit.to_le_bytes());
    }
    update_sector_crc(sector)
}

pub fn set_profile_name_optional(sector: &mut [u8], name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        return set_profile_name(sector, name);
    }
    validate_profile_sector(sector)?;
    ensure!(
        sector.len() >= PROFILE_NAME_OFFSET + PROFILE_NAME_UNITS * 2 + 2,
        "profile sector is too short for a layout-A name"
    );
    sector[PROFILE_NAME_OFFSET..PROFILE_NAME_OFFSET + PROFILE_NAME_UNITS * 2].fill(0xFF);
    update_sector_crc(sector)
}

pub fn report_rate(sector: &[u8]) -> Result<Option<u32>> {
    validate_profile_sector(sector)?;
    let interval = sector[0];
    Ok((interval != 0 && 1000 % u32::from(interval) == 0).then_some(1000 / u32::from(interval)))
}

pub fn dpi_table(sector: &[u8]) -> Result<DpiTable> {
    validate_profile_sector(sector)?;
    let levels = (0..5)
        .map(|index| u16::from_le_bytes([sector[3 + index * 2], sector[4 + index * 2]]))
        .take_while(|level| *level != 0)
        .collect::<Vec<_>>();
    ensure!(!levels.is_empty(), "profile DPI table has no levels");
    ensure!(
        usize::from(sector[1]) < levels.len(),
        "default DPI index is outside the profile table"
    );
    let shift_index = (sector[2] != 0xFF).then_some(sector[2]);
    if let Some(index) = shift_index {
        ensure!(
            usize::from(index) < levels.len(),
            "shift DPI index is outside the profile table"
        );
    }
    Ok(DpiTable {
        levels,
        default_index: sector[1],
        shift_index,
    })
}

pub fn set_dpi_table(sector: &mut [u8], table: &DpiTable) -> Result<()> {
    let shift = table
        .shift_index
        .map(|index| {
            table
                .levels
                .get(usize::from(index))
                .copied()
                .context("shift DPI index is outside the imported table")
        })
        .transpose()?;
    set_dpi(
        sector,
        &table.levels,
        usize::from(table.default_index),
        shift,
    )
}

pub fn led_slots(sector: &[u8]) -> Result<Vec<LedSlot>> {
    validate_profile_sector(sector)?;
    ensure!(
        sector.len() >= LED_SLOTS_OFFSET + LED_SLOT_COUNT * LED_SLOT_SIZE + 2,
        "profile sector is too short for layout-A LED slots"
    );
    Ok((0..LED_SLOT_COUNT)
        .map(|slot| {
            let offset = LED_SLOTS_OFFSET + slot * LED_SLOT_SIZE;
            let raw = &sector[offset..offset + LED_SLOT_SIZE];
            LedSlot {
                slot,
                effect: raw_effect_name(u16::from(raw[0])).to_owned(),
                raw_id: raw[0],
                parameters_hex: encode_hex(&raw[1..]),
                raw_hex: encode_hex(raw),
            }
        })
        .collect())
}

pub fn set_led_slot(
    sector: &mut [u8],
    slot: usize,
    effect: Effect,
    options: &EffectOptions,
) -> Result<()> {
    validate_profile_sector(sector)?;
    ensure!(slot < LED_SLOT_COUNT, "LED slot must be 0..=3");
    ensure!(
        sector.len() >= LED_SLOTS_OFFSET + LED_SLOT_COUNT * LED_SLOT_SIZE + 2,
        "profile sector is too short for layout-A LED slots"
    );
    let offset = LED_SLOTS_OFFSET + slot * LED_SLOT_SIZE;
    sector[offset] = u8::try_from(effect.raw_id).context("RGB effect id does not fit one byte")?;
    sector[offset + 1..offset + LED_SLOT_SIZE].copy_from_slice(&encode_effect(effect, options)?);
    update_sector_crc(sector)
}

fn validate_profile_sector(sector: &[u8]) -> Result<()> {
    ensure!(
        sector.len() >= MIN_PROFILE_SECTOR_SIZE,
        "profile sector is shorter than the button tables plus CRC ({MIN_PROFILE_SECTOR_SIZE} bytes)"
    );
    ensure!(sector_crc_valid(sector), "profile sector CRC is invalid");
    Ok(())
}

fn write_chunks(data: &[u8]) -> Vec<[u8; 16]> {
    // 0x8100 always transports 16-byte frames. startWrite's count limits the
    // meaningful bytes, so a partial final frame is padding only.
    data.chunks(16)
        .map(|chunk| {
            let mut frame = [0xFF_u8; 16];
            frame[..chunk.len()].copy_from_slice(chunk);
            frame
        })
        .collect()
}

pub fn crc_ccitt(data: &[u8]) -> u16 {
    let mut crc = 0xFFFF_u16;
    for byte in data {
        let temp = (crc >> 8) ^ u16::from(*byte);
        crc <<= 8;
        let mut quick = temp ^ (temp >> 4);
        crc ^= quick;
        quick <<= 5;
        crc ^= quick;
        quick <<= 7;
        crc ^= quick;
    }
    crc
}

pub fn sector_crc_valid(data: &[u8]) -> bool {
    if data.len() < 2 {
        return false;
    }
    crc_ccitt(&data[..data.len() - 2])
        == u16::from_be_bytes([data[data.len() - 2], data[data.len() - 1]])
}

pub fn update_sector_crc(data: &mut [u8]) -> Result<()> {
    ensure!(data.len() >= 2, "sector is too short for CRC");
    let crc = crc_ccitt(&data[..data.len() - 2]).to_be_bytes();
    let end = data.len();
    data[end - 2..].copy_from_slice(&crc);
    Ok(())
}

pub fn parse_binding(value: &str) -> Result<Binding> {
    let lower = value.trim().to_ascii_lowercase();
    if lower == "disabled" {
        return Ok(Binding::Disabled);
    }
    if let Some(name) = lower.strip_prefix("mouse:") {
        let button = match name {
            "left" => 1,
            "right" => 2,
            "middle" => 3,
            "back" => 4,
            "forward" => 5,
            _ => name
                .parse()
                .with_context(|| format!("unknown mouse button {name:?}"))?,
        };
        ensure!((1..=16).contains(&button), "mouse button must be 1..=16");
        return Ok(Binding::Mouse(button));
    }
    if let Some(combo) = lower.strip_prefix("key:") {
        return parse_keystroke(combo);
    }
    if let Some(name) = lower.strip_prefix("consumer:") {
        return Ok(Binding::Consumer(
            consumer_usage(name).with_context(|| format!("unknown consumer key {name:?}"))?,
        ));
    }
    if let Some(value) = lower.strip_prefix("macro:") {
        let (sector, offset) = value
            .split_once(':')
            .context("macro binding must be macro:<sector>:<offset>")?;
        return Ok(Binding::Macro {
            sector: parse_u16(sector)?,
            offset: parse_u16(offset)?,
        });
    }
    if let Some(profile) = lower.strip_prefix("enable-profile:") {
        return Ok(Binding::EnableProfile(
            profile
                .parse()
                .context("enable-profile:<n> needs a number")?,
        ));
    }
    if let Some(code) = lower.strip_prefix("special:") {
        return Ok(Binding::Special(
            parse_u16(code)?
                .try_into()
                .context("special binding code must fit one byte")?,
        ));
    }
    if let Some(raw) = lower.strip_prefix("raw:") {
        let bytes = decode_hex(raw)?;
        ensure!(bytes.len() == 4, "raw binding must contain exactly 4 bytes");
        return Ok(Binding::Other(bytes.try_into().unwrap()));
    }
    let special = match lower.as_str() {
        "noop" => 0x00,
        "tilt-left" => 0x01,
        "tilt-right" => 0x02,
        "next-dpi" | "dpi-up" => 0x03,
        "prev-dpi" | "dpi-down" => 0x04,
        "cycle-dpi" | "dpi-cycle" => 0x05,
        "default-dpi" | "dpi-default" => 0x06,
        "shift-dpi" | "dpi-shift" => 0x07,
        "next-profile" | "profile-next" => 0x08,
        "prev-profile" | "profile-prev" => 0x09,
        "cycle-profile" | "profile-cycle" => 0x0A,
        "g-shift" => 0x0B,
        "battery-indicator" => 0x0C,
        "scroll-down" => 0x10,
        "scroll-up" => 0x11,
        "wheel-mode-toggle" => 0x1C,
        "ratchet-force-cycle" => 0x1D,
        _ => bail!("invalid binding {value:?}"),
    };
    Ok(Binding::Special(special))
}

fn parse_keystroke(combo: &str) -> Result<Binding> {
    let normalized = normalize_key_name(combo);
    if !combo.contains('+')
        && let Some(usage) = consumer_usage(&normalized)
    {
        return Ok(Binding::Consumer(usage));
    }
    let (modifiers, usage) = parse_key_combo(combo)?;
    Ok(Binding::Key { modifiers, usage })
}

fn parse_u16(value: &str) -> Result<u16> {
    let result = if let Some(hex) = value.strip_prefix("0x") {
        u16::from_str_radix(hex, 16)
    } else {
        value.parse()
    };
    result.with_context(|| format!("invalid 16-bit value {value:?}"))
}

pub fn parse_key_combo(combo: &str) -> Result<(u8, u8)> {
    let mut modifiers = 0_u8;
    let mut usage = None;
    for part in combo.split('+') {
        let part = part.trim();
        ensure!(!part.is_empty(), "empty key-combo component");
        let lower = normalize_key_name(part);
        let modifier = match lower.as_str() {
            "ctrl" | "control" => Some(0x01),
            "shift" => Some(0x02),
            "alt" => Some(0x04),
            "win" | "meta" | "super" => Some(0x08),
            "rctrl" | "rightctrl" | "rightcontrol" => Some(0x10),
            "rshift" | "rightshift" => Some(0x20),
            "ralt" | "rightalt" | "altgr" => Some(0x40),
            "rwin" | "rightwin" | "rightmeta" | "rightgui" => Some(0x80),
            _ => None,
        };
        if let Some(modifier) = modifier {
            ensure!(modifiers & modifier == 0, "duplicate modifier {part:?}");
            modifiers |= modifier;
        } else {
            ensure!(
                usage.is_none(),
                "a key combo must contain exactly one non-modifier key"
            );
            usage = Some(key_usage(&lower).with_context(|| format!("unknown key {part:?}"))?);
        }
    }
    ensure!(modifiers != 0 || usage.is_some(), "key combo is empty");
    Ok((modifiers, usage.unwrap_or(0)))
}

fn normalize_key_name(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .replace(['-', '_', ' '], "")
}

fn key_usage(key: &str) -> Option<u8> {
    if let Some(hex) = key.strip_prefix("usage0x") {
        return u8::from_str_radix(hex, 16).ok();
    }
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
    {
        return match number {
            1..=12 => Some(0x3A + number - 1),
            13..=24 => Some(0x68 + number - 13),
            _ => None,
        };
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
        _ => return None,
    })
}

fn consumer_usage(value: &str) -> Option<u16> {
    let name = normalize_key_name(value);
    if let Some(hex) = name
        .strip_prefix("consumer0x")
        .or_else(|| name.strip_prefix("0x"))
    {
        return u16::from_str_radix(hex, 16).ok();
    }
    Some(match name.as_str() {
        "playpause" | "mediaplaypause" => 0x00CD,
        "stop" | "mediastop" => 0x00B7,
        "volumeup" => 0x00E9,
        "volumedown" => 0x00EA,
        "next" | "nexttrack" | "medianext" => 0x00B5,
        "previous" | "previoustrack" | "prevtrack" | "mediaprev" | "mediaprevious" => 0x00B6,
        "mute" | "volumemute" | "mediamute" => 0x00E2,
        _ => return None,
    })
}

fn format_key_combo(modifiers: u8, usage: u8) -> String {
    let mut parts = Vec::new();
    for (mask, name) in [
        (1, "ctrl"),
        (2, "shift"),
        (4, "alt"),
        (8, "win"),
        (0x10, "rctrl"),
        (0x20, "rshift"),
        (0x40, "ralt"),
        (0x80, "rwin"),
    ] {
        if modifiers & mask != 0 {
            parts.push(name.to_owned());
        }
    }
    if usage != 0 {
        parts.push(usage_name(usage));
    }
    parts.join("+")
}

fn consumer_name(usage: u16) -> String {
    match usage {
        0x00CD => "play-pause".into(),
        0x00B7 => "media-stop".into(),
        0x00E9 => "volume-up".into(),
        0x00EA => "volume-down".into(),
        0x00B5 => "next-track".into(),
        0x00B6 => "previous-track".into(),
        0x00E2 => "mute".into(),
        _ => format!("consumer-0x{usage:04X}"),
    }
}

fn usage_name(usage: u8) -> String {
    match usage {
        0x04..=0x1D => ((b'a' + usage - 0x04) as char).to_string(),
        0x1E..=0x26 => ((b'1' + usage - 0x1E) as char).to_string(),
        0x27 => "0".into(),
        0x3A..=0x45 => format!("f{}", usage - 0x3A + 1),
        0x68..=0x73 => format!("f{}", usage - 0x68 + 13),
        0x28 => "enter".into(),
        0x29 => "esc".into(),
        0x2A => "backspace".into(),
        0x2B => "tab".into(),
        0x2C => "space".into(),
        0x49 => "insert".into(),
        0x4A => "home".into(),
        0x4B => "pageup".into(),
        0x4C => "delete".into(),
        0x4D => "end".into(),
        0x4E => "pagedown".into(),
        0x4F => "right".into(),
        0x50 => "left".into(),
        0x51 => "down".into(),
        0x52 => "up".into(),
        _ => format!("usage-0x{usage:02X}"),
    }
}

fn mouse_button_name(button: u8) -> String {
    match button {
        1 => "left".into(),
        2 => "right".into(),
        3 => "middle".into(),
        4 => "back".into(),
        5 => "forward".into(),
        _ => button.to_string(),
    }
}

fn special_name(code: u8) -> String {
    match code {
        0x00 => "noop".into(),
        0x01 => "tilt-left".into(),
        0x02 => "tilt-right".into(),
        0x03 => "next-dpi".into(),
        0x04 => "prev-dpi".into(),
        0x05 => "cycle-dpi".into(),
        0x06 => "default-dpi".into(),
        0x07 => "shift-dpi".into(),
        0x08 => "next-profile".into(),
        0x09 => "prev-profile".into(),
        0x0A => "cycle-profile".into(),
        0x0B => "g-shift".into(),
        0x0C => "battery-indicator".into(),
        0x10 => "scroll-down".into(),
        0x11 => "scroll-up".into(),
        0x1C => "wheel-mode-toggle".into(),
        0x1D => "ratchet-force-cycle".into(),
        _ => format!("special:0x{code:02X}"),
    }
}

impl Macro {
    pub fn from_steps_json(value: &str) -> Result<Self> {
        let steps = serde_json::from_str(value).context("--steps must be a JSON array")?;
        let result = Self { steps };
        encode_macro(&result)?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<()> {
        for step in &self.steps {
            match step {
                MacroStep::KeyPress { key_press } => {
                    parse_key_combo(key_press)?;
                }
                MacroStep::KeyRelease { key_release } => {
                    parse_key_combo(key_release)?;
                }
                MacroStep::Key { key } => {
                    parse_key_combo(key)?;
                }
                MacroStep::Delay { .. } => {}
                MacroStep::Consumer { consumer } => {
                    consumer_usage(consumer)
                        .with_context(|| format!("unknown consumer key {consumer:?}"))?;
                }
                MacroStep::Text { text } => {
                    for character in text.chars() {
                        ascii_key(character)
                            .with_context(|| format!("onboard text contains non-ASCII or unsupported character {character:?}"))?;
                    }
                }
                MacroStep::WaitForRelease { wait_for_release } => ensure!(
                    *wait_for_release,
                    "wait_for_release must be true when present"
                ),
            }
        }
        Ok(())
    }
}

fn macro_events(value: &Macro) -> Result<Vec<Vec<u8>>> {
    value.validate()?;
    let mut events = Vec::new();
    for step in &value.steps {
        match step {
            MacroStep::KeyPress { key_press } => {
                let (modifiers, usage) = parse_key_combo(key_press)?;
                events.push(vec![0x43, modifiers, usage]);
            }
            MacroStep::KeyRelease { key_release } => {
                let (modifiers, usage) = parse_key_combo(key_release)?;
                events.push(vec![0x44, modifiers, usage]);
            }
            MacroStep::Key { key } => {
                let (modifiers, usage) = parse_key_combo(key)?;
                events.push(vec![0x43, modifiers, usage]);
                events.push(vec![0x44, modifiers, usage]);
            }
            MacroStep::Delay { delay_ms } => {
                let [high, low] = delay_ms.to_be_bytes();
                events.push(vec![0x40, high, low]);
            }
            MacroStep::Consumer { consumer } => {
                let usage = consumer_usage(consumer)
                    .with_context(|| format!("unknown consumer key {consumer:?}"))?;
                let [high, low] = usage.to_be_bytes();
                events.push(vec![0x45, high, low]);
                events.push(vec![0x46, high, low]);
            }
            MacroStep::Text { text } => {
                for character in text.chars() {
                    let (modifiers, usage) = ascii_key(character).unwrap();
                    events.push(vec![0x43, modifiers, usage]);
                    events.push(vec![0x44, modifiers, usage]);
                }
            }
            MacroStep::WaitForRelease { .. } => events.push(vec![0x01]),
        }
    }
    Ok(events)
}

pub fn encode_macro(value: &Macro) -> Result<Vec<u8>> {
    let mut bytes = macro_events(value)?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    bytes.push(0xFF);
    if bytes.len() % 2 != 0 {
        bytes.push(0xFF);
    }
    Ok(bytes)
}

fn ascii_key(character: char) -> Option<(u8, u8)> {
    if character.is_ascii_lowercase() {
        return Some((0, 0x04 + character as u8 - b'a'));
    }
    if character.is_ascii_uppercase() {
        return Some((0x02, 0x04 + character as u8 - b'A'));
    }
    if character.is_ascii_digit() {
        return key_usage(&character.to_string()).map(|usage| (0, usage));
    }
    Some(match character {
        ' ' => (0, 0x2C),
        '\n' | '\r' => (0, 0x28),
        '\t' => (0, 0x2B),
        '-' => (0, 0x2D),
        '_' => (0x02, 0x2D),
        '=' => (0, 0x2E),
        '+' => (0x02, 0x2E),
        '[' => (0, 0x2F),
        '{' => (0x02, 0x2F),
        ']' => (0, 0x30),
        '}' => (0x02, 0x30),
        '\\' => (0, 0x31),
        '|' => (0x02, 0x31),
        ';' => (0, 0x33),
        ':' => (0x02, 0x33),
        '\'' => (0, 0x34),
        '"' => (0x02, 0x34),
        '`' => (0, 0x35),
        '~' => (0x02, 0x35),
        ',' => (0, 0x36),
        '<' => (0x02, 0x36),
        '.' => (0, 0x37),
        '>' => (0x02, 0x37),
        '/' => (0, 0x38),
        '?' => (0x02, 0x38),
        '!' => (0x02, 0x1E),
        '@' => (0x02, 0x1F),
        '#' => (0x02, 0x20),
        '$' => (0x02, 0x21),
        '%' => (0x02, 0x22),
        '^' => (0x02, 0x23),
        '&' => (0x02, 0x24),
        '*' => (0x02, 0x25),
        '(' => (0x02, 0x26),
        ')' => (0x02, 0x27),
        _ => return None,
    })
}

pub fn macro_sector_ids(description: &Description, entries: &[DirectoryEntry]) -> Result<Vec<u16>> {
    let last_profile = entries.iter().map(|entry| entry.sector).max().unwrap_or(0);
    ensure!(
        last_profile < u16::from(description.sector_count),
        "profile sector 0x{last_profile:04X} is outside sector_count {}",
        description.sector_count
    );
    let profile_sectors = entries
        .iter()
        .map(|entry| entry.sector)
        .collect::<BTreeSet<_>>();
    let sectors = (last_profile + 1..u16::from(description.sector_count))
        .filter(|sector| *sector != 0 && !profile_sectors.contains(sector))
        .collect::<Vec<_>>();
    ensure!(
        !sectors.is_empty(),
        "device description has no macro sectors"
    );
    Ok(sectors)
}

pub fn pack_macros(
    macros: &[Macro],
    macro_sectors: &[u16],
    sector_size: u16,
) -> Result<PackedMacros> {
    ensure!(!macro_sectors.is_empty(), "no macro sectors are available");
    let size = usize::from(sector_size);
    ensure!(size >= 8, "macro sector is too small");
    let payload_size = size - 2;
    let mut sectors = macro_sectors
        .iter()
        .map(|sector| (*sector, vec![0xFF; size]))
        .collect::<Vec<_>>();
    let mut sector_index = 0_usize;
    let mut offset = 0_usize;
    let mut locations = Vec::with_capacity(macros.len());
    let mut last_used_sector = None;

    for (macro_index, value) in macros.iter().enumerate() {
        ensure!(offset & 1 == 0, "macro packer lost 2-byte record alignment");
        locations.push((
            sectors[sector_index].0,
            u16::try_from(offset).context("macro offset is too large")?,
        ));
        let events = macro_events(value)?;
        for (index, event) in events.iter().enumerate() {
            let next_len = events.get(index + 1).map_or(2, Vec::len);
            place_macro_event(
                &mut sectors,
                macro_sectors,
                payload_size,
                &mut sector_index,
                &mut offset,
                event,
                Some(next_len),
            )?;
        }
        place_macro_end(
            &mut sectors,
            macro_sectors,
            payload_size,
            &mut sector_index,
            &mut offset,
            macro_index + 1 < macros.len(),
        )?;
        last_used_sector = Some(sector_index);
    }
    if let Some(last) = last_used_sector {
        for (_, sector) in &mut sectors[..=last] {
            update_sector_crc(sector)?;
        }
    }
    Ok((sectors, locations))
}

fn place_macro_event(
    sectors: &mut [(u16, Vec<u8>)],
    ids: &[u16],
    payload_size: usize,
    sector_index: &mut usize,
    offset: &mut usize,
    event: &[u8],
    next_len: Option<usize>,
) -> Result<()> {
    ensure!(
        event.len() <= payload_size,
        "macro event is larger than a sector"
    );
    let remaining = payload_size - *offset;
    let after = remaining.saturating_sub(event.len());
    let must_jump =
        event.len() > remaining || (next_len.is_some() && event.len() <= remaining && after < 5);
    if must_jump {
        append_macro_jump(sectors, ids, payload_size, sector_index, offset)?;
    }
    ensure!(
        event.len() <= payload_size - *offset,
        "macro event does not fit after sector jump"
    );
    sectors[*sector_index].1[*offset..*offset + event.len()].copy_from_slice(event);
    *offset += event.len();
    Ok(())
}

fn append_macro_jump(
    sectors: &mut [(u16, Vec<u8>)],
    ids: &[u16],
    payload_size: usize,
    sector_index: &mut usize,
    offset: &mut usize,
) -> Result<()> {
    ensure!(
        payload_size - *offset >= 5,
        "insufficient space for JUMP event in macro sector 0x{:04X}",
        ids[*sector_index]
    );
    ensure!(
        *sector_index + 1 < ids.len(),
        "macro data exceeds available macro sectors"
    );
    let next = ids[*sector_index + 1].to_be_bytes();
    sectors[*sector_index].1[*offset..*offset + 5].copy_from_slice(&[0x60, next[0], next[1], 0, 0]);
    *sector_index += 1;
    *offset = 0;
    Ok(())
}

fn place_macro_end(
    sectors: &mut [(u16, Vec<u8>)],
    ids: &[u16],
    payload_size: usize,
    sector_index: &mut usize,
    offset: &mut usize,
    has_next_macro: bool,
) -> Result<()> {
    let needed = if (*offset + 1) % 2 == 1 { 2 } else { 1 };
    let remaining = payload_size - *offset;
    if remaining < needed || (has_next_macro && remaining - needed < 5) {
        append_macro_jump(sectors, ids, payload_size, sector_index, offset)?;
    }
    sectors[*sector_index].1[*offset] = 0xFF;
    *offset += 1;
    if *offset % 2 == 1 {
        sectors[*sector_index].1[*offset] = 0xFF;
        *offset += 1;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodedMacroEvent {
    KeyPress(u8, u8),
    KeyRelease(u8, u8),
    Delay(u16),
    ConsumerDown(u16),
    ConsumerUp(u16),
    WaitForRelease,
}

pub fn decode_macro(
    sectors: &BTreeMap<u16, Vec<u8>>,
    macro_sector_ids: &BTreeSet<u16>,
    start_sector: u16,
    start_offset: u16,
) -> Result<Macro> {
    ensure!(
        macro_sector_ids.contains(&start_sector),
        "macro starts outside the explicit macro-sector range"
    );
    let mut sector = start_sector;
    let mut offset = usize::from(start_offset);
    let mut visited = BTreeSet::new();
    let mut events = Vec::new();

    loop {
        ensure!(
            visited.insert((sector, offset)),
            "macro bytecode contains a loop"
        );
        let data = sectors
            .get(&sector)
            .with_context(|| format!("macro sector 0x{sector:04X} was not read"))?;
        // A blank (never-written) sector reads as an immediate END: G HUB can leave bindings
        // pointing at macro sectors it never filled (seen on a G913 G-Shift bank).
        ensure!(
            sector_intact(data),
            "macro sector 0x{sector:04X} CRC is invalid"
        );
        let payload_end = data.len() - 2;
        ensure!(
            offset < payload_end,
            "macro offset is outside sector payload"
        );
        let opcode = data[offset];
        match opcode {
            0x01 => {
                events.push(DecodedMacroEvent::WaitForRelease);
                offset += 1;
            }
            0x40 | 0x43..=0x46 => {
                ensure!(
                    offset + 3 <= payload_end,
                    "truncated macro event 0x{opcode:02X}"
                );
                let first = data[offset + 1];
                let second = data[offset + 2];
                events.push(match opcode {
                    0x40 => DecodedMacroEvent::Delay(u16::from_be_bytes([first, second])),
                    0x43 => DecodedMacroEvent::KeyPress(first, second),
                    0x44 => DecodedMacroEvent::KeyRelease(first, second),
                    0x45 => DecodedMacroEvent::ConsumerDown(u16::from_be_bytes([first, second])),
                    0x46 => DecodedMacroEvent::ConsumerUp(u16::from_be_bytes([first, second])),
                    _ => unreachable!(),
                });
                offset += 3;
            }
            0x60 => {
                ensure!(offset + 5 <= payload_end, "truncated macro JUMP event");
                ensure!(
                    data[offset + 3..offset + 5] == [0, 0],
                    "macro JUMP padding is not 0x0000"
                );
                let next = u16::from_be_bytes([data[offset + 1], data[offset + 2]]);
                ensure!(
                    macro_sector_ids.contains(&next),
                    "macro JUMP targets non-macro sector 0x{next:04X}"
                );
                sector = next;
                offset = 0;
            }
            0xFF => break,
            _ => bail!("unsupported onboard macro opcode 0x{opcode:02X}"),
        }
        ensure!(events.len() <= 65_535, "macro contains too many events");
    }
    Ok(Macro {
        steps: collapse_macro_events(&events)?,
    })
}

fn collapse_macro_events(events: &[DecodedMacroEvent]) -> Result<Vec<MacroStep>> {
    let mut steps = Vec::new();
    let mut index = 0;
    while index < events.len() {
        match events[index] {
            DecodedMacroEvent::KeyPress(modifiers, usage)
                if events.get(index + 1)
                    == Some(&DecodedMacroEvent::KeyRelease(modifiers, usage)) =>
            {
                steps.push(MacroStep::Key {
                    key: format_key_combo(modifiers, usage),
                });
                index += 1;
            }
            DecodedMacroEvent::KeyPress(modifiers, usage) => steps.push(MacroStep::KeyPress {
                key_press: format_key_combo(modifiers, usage),
            }),
            DecodedMacroEvent::KeyRelease(modifiers, usage) => steps.push(MacroStep::KeyRelease {
                key_release: format_key_combo(modifiers, usage),
            }),
            DecodedMacroEvent::Delay(delay_ms) => steps.push(MacroStep::Delay { delay_ms }),
            DecodedMacroEvent::ConsumerDown(usage)
                if events.get(index + 1) == Some(&DecodedMacroEvent::ConsumerUp(usage)) =>
            {
                steps.push(MacroStep::Consumer {
                    consumer: consumer_name(usage),
                });
                index += 1;
            }
            DecodedMacroEvent::ConsumerDown(usage) => {
                bail!("consumer-down 0x{usage:04X} is not followed by matching consumer-up")
            }
            DecodedMacroEvent::ConsumerUp(usage) => {
                bail!("consumer-up 0x{usage:04X} has no matching consumer-down")
            }
            DecodedMacroEvent::WaitForRelease => steps.push(MacroStep::WaitForRelease {
                wait_for_release: true,
            }),
        }
        index += 1;
    }
    Ok(steps)
}

fn encode_hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02X}")).collect()
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    let compact = value
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    ensure!(
        compact.len() % 2 == 0,
        "hex string has an odd number of digits"
    );
    compact
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(text, 16).with_context(|| format!("invalid hex byte {text:?}"))
        })
        .collect()
}

pub fn export_state(dump: &SectorDump, device_type: &str) -> Result<OnboardExport> {
    let parsed_description = Description::parse(dump.description.raw)?;
    ensure!(
        parsed_description == dump.description,
        "description fields do not match description.raw"
    );
    let sectors = sector_map(dump)?;
    let directory_raw = sectors
        .get(&0)
        .context("onboard sector set has no directory sector 0")?;
    let entries = parse_directory(directory_raw, dump.description.profile_count)?;
    let macro_ids = macro_sector_ids(&dump.description, &entries)?;
    let macro_id_set = macro_ids.iter().copied().collect::<BTreeSet<_>>();
    let mut profiles = Vec::with_capacity(entries.len());

    for entry in &entries {
        profiles.push(decode_export_profile(
            &dump.description,
            entry,
            device_type.eq_ignore_ascii_case("KEYBOARD"),
            &sectors,
            &macro_id_set,
        )?);
    }

    Ok(OnboardExport {
        version: EXPORT_VERSION,
        device_type: device_type.to_ascii_uppercase(),
        description: dump.description.clone(),
        directory: ExportDirectory {
            entries,
            raw_sector_hex: encode_hex(directory_raw),
        },
        profiles,
        macro_sectors: macro_ids
            .into_iter()
            .map(|sector| {
                Ok(RawSector {
                    sector,
                    raw_hex: encode_hex(
                        sectors
                            .get(&sector)
                            .with_context(|| format!("macro sector 0x{sector:04X} was not read"))?,
                    ),
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

fn decode_export_profile(
    description: &Description,
    entry: &DirectoryEntry,
    keyboard: bool,
    sectors: &BTreeMap<u16, Vec<u8>>,
    macro_ids: &BTreeSet<u16>,
) -> Result<ExportProfile> {
    let sector = sectors
        .get(&entry.sector)
        .with_context(|| format!("profile sector 0x{:04X} was not read", entry.sector))?;
    validate_profile_sector(sector)?;
    let count = BUTTON_COUNT.min(usize::from(description.button_count));
    Ok(ExportProfile {
        index: entry.index,
        sector: entry.sector,
        enabled: entry.enabled,
        name: profile_name(sector)?,
        rate_hz: report_rate(sector)?,
        dpi: if keyboard {
            None
        } else {
            match dpi_table(sector) {
                Ok(table) => Some(table),
                Err(_) if !entry.enabled => None,
                Err(error) => return Err(error),
            }
        },
        bindings: export_binding_bank(sector, BUTTONS_OFFSET, count, keyboard, sectors, macro_ids)?,
        gshift_bindings: export_binding_bank(
            sector,
            GSHIFT_BUTTONS_OFFSET,
            count,
            keyboard,
            sectors,
            macro_ids,
        )?,
        led_slots: led_slots(sector)?,
        raw_sector_hex: encode_hex(sector),
    })
}

fn export_binding_bank(
    sector: &[u8],
    base: usize,
    count: usize,
    keyboard: bool,
    sectors: &BTreeMap<u16, Vec<u8>>,
    macro_ids: &BTreeSet<u16>,
) -> Result<Vec<ExportBinding>> {
    (0..count)
        .map(|index| {
            let offset = base + index * 4;
            let raw: [u8; 4] = sector[offset..offset + 4].try_into().unwrap();
            let binding = Binding::decode(raw);
            let r#macro = match binding {
                Binding::Macro { sector, offset } => {
                    Some(decode_macro(sectors, macro_ids, sector, offset)?)
                }
                _ => None,
            };
            Ok(ExportBinding {
                number: index + 1,
                control: if keyboard {
                    format!("g{}", index + 1)
                } else {
                    format!("button {}", index + 1)
                },
                binding: binding.to_string(),
                raw_hex: encode_hex(&raw),
                r#macro,
            })
        })
        .collect()
}

pub fn save_export(path: &Path, export: &OnboardExport) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut bytes =
        serde_json::to_vec_pretty(export).context("failed to serialize onboard JSON")?;
    bytes.push(b'\n');
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

pub fn load_export(path: &Path) -> Result<OnboardExport> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse onboard JSON {}", path.display()))
}

pub fn repack_export_macros(export: &mut OnboardExport) -> Result<()> {
    let macro_ids = macro_sector_ids(&export.description, &export.directory.entries)?;
    let mut macros = Vec::new();
    let mut targets = Vec::new();
    for (profile_index, profile) in export.profiles.iter().enumerate() {
        for (gshift, bindings) in [(false, &profile.bindings), (true, &profile.gshift_bindings)] {
            for (binding_index, binding) in bindings.iter().enumerate() {
                if matches!(parse_binding(&binding.binding)?, Binding::Macro { .. }) {
                    macros.push(binding.r#macro.clone().with_context(|| {
                        format!(
                            "profile {} {}{} has a macro binding without decoded steps",
                            profile.index,
                            if gshift { "G-Shift " } else { "" },
                            binding.control
                        )
                    })?);
                    targets.push((profile_index, gshift, binding_index));
                }
            }
        }
    }
    let (sectors, locations) = pack_macros(&macros, &macro_ids, export.description.sector_size)?;
    for ((profile_index, gshift, binding_index), (sector, offset)) in
        targets.into_iter().zip(locations)
    {
        let bindings = if gshift {
            &mut export.profiles[profile_index].gshift_bindings
        } else {
            &mut export.profiles[profile_index].bindings
        };
        let binding = Binding::Macro { sector, offset };
        bindings[binding_index].binding = binding.to_string();
        bindings[binding_index].raw_hex = encode_hex(&binding.encode()?);
    }
    for profile in &mut export.profiles {
        if profile.raw_sector_hex.is_empty() {
            continue;
        }
        let mut sector = decode_hex(&profile.raw_sector_hex)?;
        ensure!(
            sector.len() == usize::from(export.description.sector_size),
            "profile {} raw sector has the wrong size",
            profile.index
        );
        let original = sector.clone();
        for (base, bindings) in [
            (BUTTONS_OFFSET, &profile.bindings),
            (GSHIFT_BUTTONS_OFFSET, &profile.gshift_bindings),
        ] {
            for (index, binding) in bindings.iter().enumerate() {
                let imported = parse_binding(&binding.binding)?;
                ensure!(
                    matches!(imported, Binding::Macro { .. }) == binding.r#macro.is_some(),
                    "{} macro steps do not match its binding",
                    binding.control
                );
                let offset = base + index * 4;
                let raw = Binding::decode(sector[offset..offset + 4].try_into().unwrap());
                if raw != imported {
                    sector[offset..offset + 4].copy_from_slice(&imported.encode()?);
                }
            }
        }
        if sector != original {
            update_sector_crc(&mut sector)?;
            profile.raw_sector_hex = encode_hex(&sector);
        }
    }
    export.macro_sectors = sectors
        .into_iter()
        .map(|(sector, bytes)| RawSector {
            sector,
            raw_hex: encode_hex(&bytes),
        })
        .collect();
    Ok(())
}

pub fn import_plan(
    exported: &OnboardExport,
    current: &SectorDump,
    device_type: &str,
) -> Result<Vec<SectorDiff>> {
    ensure!(
        exported.version == EXPORT_VERSION,
        "unsupported onboard export version {}",
        exported.version
    );
    ensure!(
        exported.device_type.eq_ignore_ascii_case(device_type),
        "export device type {} does not match target {device_type}",
        exported.device_type
    );
    ensure!(
        exported.description.raw == current.description.raw,
        "export description does not match the target device"
    );
    ensure!(
        Description::parse(exported.description.raw)? == exported.description,
        "export description fields do not match description.raw"
    );
    let current_sectors = sector_map(current)?;
    let mut normalized = exported.clone();
    let raw = raw_export_sector_map(&normalized)?;
    let mut desired = raw.clone();
    patch_export_macros(&mut normalized, &raw, &mut desired)?;
    reencode_directory(&normalized, &mut desired)?;
    reencode_profiles(&normalized, &raw, &mut desired)?;
    ensure!(
        desired.keys().eq(current_sectors.keys()),
        "export sector set does not match the target sector set"
    );
    Ok(desired
        .into_iter()
        .filter_map(|(sector, replacement)| {
            let current = current_sectors.get(&sector).unwrap();
            (current != &replacement).then(|| SectorDiff {
                sector,
                current_crc: sector_crc(current),
                replacement_crc: sector_crc(&replacement),
                current: current.clone(),
                replacement,
            })
        })
        .collect::<Vec<_>>())
}

fn patch_export_macros(
    export: &mut OnboardExport,
    raw: &BTreeMap<u16, Vec<u8>>,
    desired: &mut BTreeMap<u16, Vec<u8>>,
) -> Result<()> {
    let macro_ids = macro_sector_ids(&export.description, &export.directory.entries)?;
    let macro_id_set = macro_ids.iter().copied().collect::<BTreeSet<_>>();
    let mut pending = Vec::new();
    let mut protected = BTreeSet::new();

    for (profile_index, profile) in export.profiles.iter().enumerate() {
        for (gshift, bindings) in [(false, &profile.bindings), (true, &profile.gshift_bindings)] {
            for (binding_index, binding) in bindings.iter().enumerate() {
                match parse_binding(&binding.binding)? {
                    Binding::Macro { sector, offset } => {
                        let expected = binding.r#macro.as_ref().with_context(|| {
                            format!(
                                "profile {} {}{} has a macro binding without decoded steps",
                                profile.index,
                                if gshift { "G-Shift " } else { "" },
                                binding.control
                            )
                        })?;
                        match decode_macro(raw, &macro_id_set, sector, offset) {
                            Ok(actual) if actual == *expected => {
                                protected.extend(macro_storage_sectors(
                                    raw,
                                    &macro_id_set,
                                    sector,
                                    offset,
                                )?);
                            }
                            _ => pending.push((
                                profile_index,
                                gshift,
                                binding_index,
                                expected.clone(),
                            )),
                        }
                    }
                    _ => ensure!(
                        binding.r#macro.is_none(),
                        "{} has macro steps but is not macro-bound",
                        binding.control
                    ),
                }
            }
        }
    }

    // Changed/new macros use unreferenced blank sectors first, leaving every existing
    // unchanged macro (including an empty dangling reference) at its original address.
    // If those blanks are insufficient, only that profile's macros are re-packed, and
    // sectors referenced by other profiles are excluded from the fallback pack.
    for profile_index in 0..export.profiles.len() {
        let edits = pending
            .iter()
            .filter(|(index, _, _, _)| *index == profile_index)
            .cloned()
            .collect::<Vec<_>>();
        if edits.is_empty() {
            continue;
        }
        let available = macro_ids
            .iter()
            .copied()
            .filter(|sector| {
                !protected.contains(sector)
                    && desired
                        .get(sector)
                        .map_or(false, |bytes| bytes.iter().all(|byte| *byte == 0xFF))
            })
            .collect::<Vec<_>>();
        let values = edits
            .iter()
            .map(|(_, _, _, value)| value.clone())
            .collect::<Vec<_>>();
        if !available.is_empty()
            && let Ok((packed, locations)) =
                pack_macros(&values, &available, export.description.sector_size)
        {
            for (sector, bytes) in packed {
                if !bytes.iter().all(|byte| *byte == 0xFF) {
                    desired.insert(sector, bytes);
                }
            }
            for ((_, gshift, binding_index, _), (sector, offset)) in
                edits.into_iter().zip(locations)
            {
                set_export_macro_location(
                    export,
                    profile_index,
                    gshift,
                    binding_index,
                    sector,
                    offset,
                )?;
            }
            continue;
        }

        let mut protected_by_other_profiles = BTreeSet::new();
        for (other_index, profile) in export.profiles.iter().enumerate() {
            if other_index == profile_index {
                continue;
            }
            for bindings in [&profile.bindings, &profile.gshift_bindings] {
                for binding in bindings {
                    if let Binding::Macro { sector, offset } = parse_binding(&binding.binding)?
                        && let Ok(sectors) =
                            macro_storage_sectors(desired, &macro_id_set, sector, offset)
                    {
                        protected_by_other_profiles.extend(sectors);
                    }
                }
            }
        }
        let candidates = macro_ids
            .iter()
            .copied()
            .filter(|sector| !protected_by_other_profiles.contains(sector))
            .collect::<Vec<_>>();
        let mut all_targets = Vec::new();
        let profile = &export.profiles[profile_index];
        for (gshift, bindings) in [(false, &profile.bindings), (true, &profile.gshift_bindings)] {
            for (binding_index, binding) in bindings.iter().enumerate() {
                if matches!(parse_binding(&binding.binding)?, Binding::Macro { .. }) {
                    all_targets.push((
                        gshift,
                        binding_index,
                        binding.r#macro.clone().with_context(|| {
                            format!(
                                "{} has a macro binding without decoded steps",
                                binding.control
                            )
                        })?,
                    ));
                }
            }
        }
        let values = all_targets
            .iter()
            .map(|(_, _, value)| value.clone())
            .collect::<Vec<_>>();
        let (packed, locations) = pack_macros(&values, &candidates, export.description.sector_size)
            .with_context(|| {
                format!(
                    "profile {} macros do not fit without moving another profile's macros",
                    export.profiles[profile_index].index
                )
            })?;
        for (sector, bytes) in packed {
            if !bytes.iter().all(|byte| *byte == 0xFF) {
                desired.insert(sector, bytes);
            }
        }
        for ((gshift, binding_index, _), (sector, offset)) in all_targets.into_iter().zip(locations)
        {
            set_export_macro_location(
                export,
                profile_index,
                gshift,
                binding_index,
                sector,
                offset,
            )?;
        }
    }
    Ok(())
}

fn set_export_macro_location(
    export: &mut OnboardExport,
    profile_index: usize,
    gshift: bool,
    binding_index: usize,
    sector: u16,
    offset: u16,
) -> Result<()> {
    let bindings = if gshift {
        &mut export.profiles[profile_index].gshift_bindings
    } else {
        &mut export.profiles[profile_index].bindings
    };
    let binding = Binding::Macro { sector, offset };
    bindings[binding_index].binding = binding.to_string();
    bindings[binding_index].raw_hex = encode_hex(&binding.encode()?);
    Ok(())
}

fn macro_storage_sectors(
    sectors: &BTreeMap<u16, Vec<u8>>,
    macro_ids: &BTreeSet<u16>,
    start_sector: u16,
    start_offset: u16,
) -> Result<BTreeSet<u16>> {
    ensure!(
        macro_ids.contains(&start_sector),
        "macro starts outside the explicit macro-sector range"
    );
    let mut sector = start_sector;
    let mut offset = usize::from(start_offset);
    let mut visited = BTreeSet::new();
    let mut used = BTreeSet::new();
    loop {
        ensure!(
            visited.insert((sector, offset)),
            "macro bytecode contains a loop"
        );
        let data = sectors
            .get(&sector)
            .with_context(|| format!("macro sector 0x{sector:04X} was not read"))?;
        ensure!(
            sector_intact(data),
            "macro sector 0x{sector:04X} CRC is invalid"
        );
        let payload_end = data.len() - 2;
        ensure!(
            offset < payload_end,
            "macro offset is outside sector payload"
        );
        used.insert(sector);
        match data[offset] {
            0x01 => offset += 1,
            0x40 | 0x43..=0x46 => {
                ensure!(offset + 3 <= payload_end, "truncated macro event");
                offset += 3;
            }
            0x60 => {
                ensure!(offset + 5 <= payload_end, "truncated macro JUMP event");
                ensure!(
                    data[offset + 3..offset + 5] == [0, 0],
                    "macro JUMP padding is not 0x0000"
                );
                let next = u16::from_be_bytes([data[offset + 1], data[offset + 2]]);
                ensure!(
                    macro_ids.contains(&next),
                    "macro JUMP targets non-macro sector 0x{next:04X}"
                );
                sector = next;
                offset = 0;
            }
            0xFF => break,
            opcode => bail!("unsupported onboard macro opcode 0x{opcode:02X}"),
        }
    }
    Ok(used)
}

fn raw_export_sector_map(export: &OnboardExport) -> Result<BTreeMap<u16, Vec<u8>>> {
    let size = usize::from(export.description.sector_size);
    let mut sectors = BTreeMap::new();
    insert_export_sector(
        &mut sectors,
        0,
        decode_hex(&export.directory.raw_sector_hex)?,
        size,
    )?;
    for profile in &export.profiles {
        insert_export_sector(
            &mut sectors,
            profile.sector,
            decode_hex(&profile.raw_sector_hex)?,
            size,
        )?;
    }
    for macro_sector in &export.macro_sectors {
        insert_export_sector(
            &mut sectors,
            macro_sector.sector,
            decode_hex(&macro_sector.raw_hex)?,
            size,
        )?;
    }
    Ok(sectors)
}

fn insert_export_sector(
    sectors: &mut BTreeMap<u16, Vec<u8>>,
    sector: u16,
    bytes: Vec<u8>,
    size: usize,
) -> Result<()> {
    ensure!(
        bytes.len() == size,
        "sector 0x{sector:04X} has the wrong size"
    );
    ensure!(
        sector_intact(&bytes),
        "sector 0x{sector:04X} CRC is invalid"
    );
    ensure!(
        sectors.insert(sector, bytes).is_none(),
        "export contains duplicate sector 0x{sector:04X}"
    );
    Ok(())
}

fn reencode_directory(export: &OnboardExport, sectors: &mut BTreeMap<u16, Vec<u8>>) -> Result<()> {
    ensure!(
        export.directory.entries.len() == usize::from(export.description.profile_count),
        "export directory entry count is incorrect"
    );
    let directory = sectors.get_mut(&0).unwrap();
    let raw_entries = parse_directory(directory, export.description.profile_count)?;
    let original = directory.clone();
    for (index, (entry, raw)) in export.directory.entries.iter().zip(raw_entries).enumerate() {
        ensure!(
            entry.index == index + 1
                && (1..=u16::from(export.description.profile_count)).contains(&entry.sector),
            "export directory entry {} has an invalid index or sector",
            index + 1
        );
        if (entry.sector, entry.enabled) != (raw.sector, raw.enabled) {
            let offset = index * 4;
            directory[offset..offset + 2].copy_from_slice(&entry.sector.to_be_bytes());
            directory[offset + 2] = u8::from(entry.enabled);
            directory[offset + 3] = 0xFF;
        }
    }
    if directory != &original {
        update_sector_crc(directory)?;
    }
    parse_directory(directory, export.description.profile_count)?;
    Ok(())
}

fn reencode_profiles(
    export: &OnboardExport,
    raw: &BTreeMap<u16, Vec<u8>>,
    sectors: &mut BTreeMap<u16, Vec<u8>>,
) -> Result<()> {
    let keyboard = export.device_type.eq_ignore_ascii_case("KEYBOARD");
    let count = BUTTON_COUNT.min(usize::from(export.description.button_count));
    ensure!(
        export.profiles.len() == export.directory.entries.len(),
        "export profile count does not match directory"
    );
    let raw_entries = parse_directory(
        raw.get(&0)
            .context("onboard export has no raw directory sector")?,
        export.description.profile_count,
    )?;
    let macro_ids = macro_sector_ids(&export.description, &export.directory.entries)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    for (position, (profile, entry)) in export
        .profiles
        .iter()
        .zip(&export.directory.entries)
        .enumerate()
    {
        ensure!(
            profile.index == entry.index
                && profile.sector == entry.sector
                && profile.enabled == entry.enabled,
            "export profile {} does not match its directory entry",
            profile.index
        );
        ensure!(
            profile.bindings.len() == count && profile.gshift_bindings.len() == count,
            "profile {} binding count does not match device button_count",
            profile.index
        );
        let raw_entry = DirectoryEntry {
            index: profile.index,
            sector: profile.sector,
            enabled: raw_entries[position].enabled,
        };
        let decoded =
            decode_export_profile(&export.description, &raw_entry, keyboard, raw, &macro_ids)?;
        let sector = sectors.get_mut(&profile.sector).unwrap();
        let original = sector.clone();
        if profile.name != decoded.name {
            set_profile_name_optional(sector, profile.name.as_deref())?;
        }
        if profile.rate_hz != decoded.rate_hz {
            if let Some(rate) = profile.rate_hz {
                set_rate(sector, rate, export.description.profile_format_id)?;
            } else {
                sector[0] = 0;
            }
        }
        match (&profile.dpi, &decoded.dpi, keyboard) {
            (None, None, true) => {}
            (None, Some(_), true) => unreachable!(),
            (Some(_), _, true) => bail!("keyboard export must not contain a DPI table"),
            (desired, current, false) if desired == current => {}
            (Some(table), _, false) => set_dpi_table(sector, table)?,
            (None, Some(_), false) => {
                ensure!(
                    !profile.enabled,
                    "enabled mouse profile {} cannot omit its DPI table",
                    profile.index
                );
                sector[1..13].fill(0xFF);
            }
            (None, None, false) => {}
        }
        apply_export_bindings(
            sector,
            &profile.bindings,
            &decoded.bindings,
            false,
            export.description.button_count,
        )?;
        apply_export_bindings(
            sector,
            &profile.gshift_bindings,
            &decoded.gshift_bindings,
            true,
            export.description.button_count,
        )?;
        ensure!(
            profile.led_slots.len() == LED_SLOT_COUNT,
            "profile {} must contain four LED slots",
            profile.index
        );
        for (slot, (led, raw_led)) in profile.led_slots.iter().zip(&decoded.led_slots).enumerate() {
            ensure!(led.slot == slot, "LED slot indices must be 0..=3 in order");
            let raw = decode_hex(&led.raw_hex)?;
            ensure!(raw.len() == LED_SLOT_SIZE, "LED slot must contain 11 bytes");
            let decoded_led = LedSlot {
                slot,
                effect: raw_effect_name(u16::from(raw[0])).to_owned(),
                raw_id: raw[0],
                parameters_hex: encode_hex(&raw[1..]),
                raw_hex: encode_hex(&raw),
            };
            ensure!(
                decoded_led == *led,
                "LED slot {} decoded fields do not match raw_hex",
                led.slot
            );
            if led != raw_led {
                let offset = LED_SLOTS_OFFSET + slot * LED_SLOT_SIZE;
                sector[offset..offset + LED_SLOT_SIZE].copy_from_slice(&raw);
            }
        }
        if sector != &original {
            update_sector_crc(sector)?;
        }
    }
    Ok(())
}

fn apply_export_bindings(
    sector: &mut [u8],
    bindings: &[ExportBinding],
    raw_bindings: &[ExportBinding],
    gshift: bool,
    button_count: u8,
) -> Result<()> {
    for (index, (binding, raw_binding)) in bindings.iter().zip(raw_bindings).enumerate() {
        ensure!(
            binding.number == index + 1
                && binding.number == raw_binding.number
                && binding.control == raw_binding.control,
            "binding identity must match the raw profile"
        );
        let imported = parse_binding(&binding.binding)?;
        ensure!(
            matches!(imported, Binding::Macro { .. }) == binding.r#macro.is_some(),
            "{} macro steps do not match its binding",
            binding.control
        );
        let raw = decode_hex(&raw_binding.raw_hex)?;
        ensure!(raw.len() == 4, "binding raw_hex must contain four bytes");
        let decoded = Binding::decode(raw.as_slice().try_into().unwrap());
        if imported != decoded {
            set_button(sector, binding.number, gshift, &imported, button_count)?;
        }
    }
    Ok(())
}

fn sector_map(dump: &SectorDump) -> Result<BTreeMap<u16, Vec<u8>>> {
    let expected = (0..u16::from(dump.description.sector_count)).collect::<BTreeSet<_>>();
    let sectors = dump.sectors.iter().cloned().collect::<BTreeMap<_, _>>();
    ensure!(
        sectors.len() == dump.sectors.len(),
        "sector set contains duplicate sector ids"
    );
    ensure!(
        sectors.keys().copied().collect::<BTreeSet<_>>() == expected,
        "sector set is incomplete for sector_count {}",
        dump.description.sector_count
    );
    for (sector, bytes) in &sectors {
        ensure!(
            bytes.len() == usize::from(dump.description.sector_size),
            "sector 0x{sector:04X} has the wrong size"
        );
        ensure!(sector_intact(bytes), "sector 0x{sector:04X} CRC is invalid");
    }
    Ok(sectors)
}

fn sector_crc(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[bytes.len() - 2], bytes[bytes.len() - 1]])
}

pub fn encode_dump(dump: &SectorDump) -> Result<Vec<u8>> {
    ensure!(
        dump.sectors.len() <= usize::from(u16::MAX),
        "too many sectors in dump"
    );
    let mut output = Vec::new();
    output.extend_from_slice(DUMP_MAGIC);
    output.extend_from_slice(&dump.description.raw);
    output.extend_from_slice(&(dump.sectors.len() as u16).to_be_bytes());
    for (sector, data) in &dump.sectors {
        ensure!(
            data.len() == usize::from(dump.description.sector_size),
            "dump sector size mismatch"
        );
        ensure!(
            sector_intact(data),
            "refusing to encode sector 0x{sector:04X} with invalid CRC"
        );
        output.extend_from_slice(&sector.to_be_bytes());
        output.extend_from_slice(data);
    }
    Ok(output)
}

pub fn decode_dump(data: &[u8]) -> Result<SectorDump> {
    ensure!(
        data.len() >= 26 && &data[..8] == DUMP_MAGIC,
        "invalid onboard dump header"
    );
    let raw: [u8; 16] = data[8..24].try_into().unwrap();
    let description = Description::parse(raw)?;
    let count = usize::from(u16::from_be_bytes([data[24], data[25]]));
    let record_size = 2 + usize::from(description.sector_size);
    ensure!(
        data.len() == 26 + count * record_size,
        "onboard dump length is invalid"
    );
    let mut sectors = Vec::with_capacity(count);
    let mut seen = BTreeSet::new();
    let mut offset = 26;
    for _ in 0..count {
        let sector = u16::from_be_bytes([data[offset], data[offset + 1]]);
        ensure!(
            seen.insert(sector),
            "dump contains duplicate sector 0x{sector:04X}"
        );
        offset += 2;
        let bytes = data[offset..offset + usize::from(description.sector_size)].to_vec();
        ensure!(
            sector_intact(&bytes),
            "dump sector 0x{sector:04X} has an invalid CRC"
        );
        sectors.push((sector, bytes));
        offset += usize::from(description.sector_size);
    }
    let directory = sectors
        .iter()
        .find(|(sector, _)| *sector == 0)
        .map(|(_, bytes)| bytes)
        .context("dump does not contain profile directory sector 0")?;
    let mut profile_only = BTreeSet::from([0_u16]);
    profile_only.extend(
        parse_directory(directory, description.profile_count)?
            .into_iter()
            .map(|entry| entry.sector),
    );
    let all_sectors = (0..u16::from(description.sector_count)).collect::<BTreeSet<_>>();
    ensure!(
        seen == profile_only || seen == all_sectors,
        "dump sector set is neither a legacy profile-only backup nor a complete device backup"
    );
    Ok(SectorDump {
        description,
        sectors,
    })
}

pub fn backup_path() -> Result<PathBuf> {
    let appdata = env::var_os("APPDATA").context("APPDATA is not set")?;
    Ok(PathBuf::from(appdata)
        .join("better-logihub")
        .join("last-onboard-dump.bin"))
}

pub fn save_dump(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

pub fn load_dump(path: &Path) -> Result<SectorDump> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    decode_dump(&bytes)
}

pub fn require_backup(description: &Description) -> Result<()> {
    let path = backup_path()?;
    let dump = load_dump(&path).with_context(|| format!("run `logihub onboard dump --out <file>` before writing (safety backup {} is missing or invalid)", path.display()))?;
    ensure!(
        dump.description.raw == description.raw,
        "safety backup does not match this device description; run onboard dump again"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_round_trips() {
        for binding in [
            Binding::Mouse(1),
            Binding::Mouse(5),
            Binding::Key {
                modifiers: 3,
                usage: 0x06,
            },
            Binding::Consumer(0x00E9),
            Binding::Macro {
                sector: 0x002A,
                offset: 0x1234,
            },
            Binding::Special(7),
            Binding::EnableProfile(3),
            Binding::Disabled,
        ] {
            assert_eq!(Binding::decode(binding.encode().unwrap()), binding);
        }
    }

    #[test]
    fn accepts_real_g502x_255_byte_description() {
        let raw = [
            0x01, 0x03, 0x01, 0x05, 0x02, 0x0B, 0x10, 0x00, 0xFF, 0x0A, 0x04, 0, 0, 0, 0, 0,
        ];
        let description = Description::parse(raw).unwrap();
        assert_eq!(description.sector_size, 255);
        assert_eq!(description.button_count, 11);
    }

    #[test]
    fn accepts_ten_byte_description_and_warns_on_unknown_models() {
        let response = [0x22, 0x04, 0x33, 5, 2, 11, 16, 0, 0xFF, 0x0A];
        let description = Description::parse_response(&response).unwrap();
        assert_eq!(description.memory_model_id, 0x22);
        assert_eq!(description.macro_format_id, 0x33);
        assert_eq!(description.sector_size, 255);
        assert_eq!(&description.raw[..10], &response);
        assert!(description.raw[10..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn parses_rom_profile_and_crc_response_as_big_endian() {
        assert_eq!(parse_current_profile(&[0x01, 0x03]).unwrap(), 0x0103);
        assert_eq!(crc_request(0x1234), [0x12, 0x34]);
        assert_eq!(
            execute_macro_request(0x1234, 0xABCD),
            [0x12, 0x34, 0xAB, 0xCD]
        );
        let mut raw = [0_u8; 16];
        raw[..4].copy_from_slice(&[0xAB, 0xCD, 0x12, 0x34]);
        let response = parse_crc_response(raw);
        assert_eq!(response.crc, 0xABCD);
        assert_eq!(response.raw, raw);
    }

    #[test]
    fn parses_key_combos_case_insensitively() {
        assert_eq!(parse_key_combo("ctrl+c").unwrap(), (1, 0x06));
        assert_eq!(parse_key_combo("WIN+L").unwrap(), (8, 0x0F));
        assert_eq!(parse_key_combo("f11").unwrap(), (0, 0x44));
        assert_eq!(parse_key_combo("Ctrl+Shift+Esc").unwrap(), (3, 0x29));
        assert_eq!(parse_key_combo("right-ctrl+f5").unwrap(), (0x10, 0x3E));
        assert_eq!(parse_key_combo("usage_0x64").unwrap(), (0, 0x64));
        assert_eq!(
            parse_binding("key:volume_up").unwrap(),
            Binding::Consumer(0x00E9)
        );
        assert_eq!(
            parse_binding("consumer:play-pause").unwrap(),
            Binding::Consumer(0x00CD)
        );
        assert_eq!(
            parse_binding("key:ctrl").unwrap(),
            Binding::Key {
                modifiers: 1,
                usage: 0
            }
        );
        assert!(parse_key_combo("ctrl+").is_err());
        assert!(parse_key_combo("ctrl+c+d").is_err());
        assert!(parse_key_combo("hyper+c").is_err());
    }

    #[test]
    fn crc_matches_known_ccitt_false_vector() {
        assert_eq!(crc_ccitt(b"123456789"), 0x29B1);
    }

    #[test]
    fn rewrites_button_and_crc_at_byte_level() {
        let mut sector = vec![0xFF; 255];
        update_sector_crc(&mut sector).unwrap();
        set_button(
            &mut sector,
            2,
            false,
            &Binding::Key {
                modifiers: 1,
                usage: 0x06,
            },
            11,
        )
        .unwrap();
        assert_eq!(&sector[36..40], &[0x80, 0x02, 0x01, 0x06]);
        assert!(sector_crc_valid(&sector));
        assert_eq!(
            u16::from_be_bytes([sector[253], sector[254]]),
            crc_ccitt(&sector[..253])
        );
    }

    #[test]
    fn clamps_writable_buttons_to_device_count() {
        let mut sector = vec![0xFF; 255];
        update_sector_crc(&mut sector).unwrap();
        assert!(set_button(&mut sector, 11, false, &Binding::Disabled, 11).is_ok());
        assert!(set_button(&mut sector, 12, false, &Binding::Disabled, 11).is_err());
        assert!(set_button(&mut sector, 1, false, &Binding::Disabled, 0).is_err());
    }

    #[test]
    fn splits_255_byte_write_into_16_padded_frames() {
        let data = (0..255).map(|value| value as u8).collect::<Vec<_>>();
        let chunks = write_chunks(&data);
        assert_eq!(chunks.len(), 16);
        assert_eq!(&chunks[0], &data[..16]);
        assert_eq!(&chunks[15][..15], &data[240..255]);
        assert_eq!(chunks[15][15], 0xFF);
    }

    #[test]
    fn validates_complete_directory_layout() {
        let mut directory = vec![0xFF; 255];
        directory[..22].copy_from_slice(&[
            0, 1, 1, 0, 0, 2, 1, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0xFF, 0xFF,
        ]);
        update_sector_crc(&mut directory).unwrap();
        assert_eq!(parse_directory(&directory, 5).unwrap().len(), 5);

        let mut bad_high = directory.clone();
        bad_high[0] = 1;
        update_sector_crc(&mut bad_high).unwrap();
        assert!(parse_directory(&bad_high, 5).is_err());

        let mut bad_low = directory.clone();
        bad_low[1] = 6;
        update_sector_crc(&mut bad_low).unwrap();
        assert!(parse_directory(&bad_low, 5).is_err());

        let mut bad_terminator = directory.clone();
        bad_terminator[20] = 0;
        update_sector_crc(&mut bad_terminator).unwrap();
        assert!(parse_directory(&bad_terminator, 5).is_err());
    }

    #[test]
    fn profile_name_is_utf16le_and_ff_means_unnamed() {
        let mut sector = vec![0xFF; 255];
        update_sector_crc(&mut sector).unwrap();
        assert_eq!(profile_name(&sector).unwrap(), None);

        set_profile_name(&mut sector, "日本語").unwrap();
        assert_eq!(profile_name(&sector).unwrap().as_deref(), Some("日本語"));
        assert_eq!(
            &sector[PROFILE_NAME_OFFSET..PROFILE_NAME_OFFSET + 6],
            &[0xE5, 0x65, 0x2C, 0x67, 0x9E, 0x8A]
        );
        assert!(sector_crc_valid(&sector));
    }

    #[test]
    fn parses_all_special_bindings_and_macro_offsets() {
        let specials = [
            ("noop", 0x00),
            ("tilt-left", 0x01),
            ("tilt-right", 0x02),
            ("next-dpi", 0x03),
            ("prev-dpi", 0x04),
            ("cycle-dpi", 0x05),
            ("default-dpi", 0x06),
            ("shift-dpi", 0x07),
            ("next-profile", 0x08),
            ("prev-profile", 0x09),
            ("cycle-profile", 0x0A),
            ("g-shift", 0x0B),
            ("battery-indicator", 0x0C),
            ("scroll-down", 0x10),
            ("scroll-up", 0x11),
            ("wheel-mode-toggle", 0x1C),
            ("ratchet-force-cycle", 0x1D),
        ];
        for (name, code) in specials {
            assert_eq!(parse_binding(name).unwrap(), Binding::Special(code));
            assert_eq!(special_name(code), name);
        }
        let macro_binding = Binding::decode([0x00, 0x2A, 0x12, 0x34]);
        assert_eq!(
            macro_binding,
            Binding::Macro {
                sector: 0x002A,
                offset: 0x1234
            }
        );
        assert_eq!(macro_binding.encode().unwrap(), [0x00, 0x2A, 0x12, 0x34]);
        assert_eq!(parse_binding("macro:0x2a:4660").unwrap(), macro_binding);
        let enable_profile = Binding::decode([0x90, 0x0D, 0xFF, 3]);
        assert_eq!(enable_profile, Binding::EnableProfile(3));
        assert_eq!(enable_profile.to_string(), "enable-profile:3");
        assert_eq!(parse_binding("enable-profile:3").unwrap(), enable_profile);
    }

    #[test]
    fn report_rate_rejects_non_layout_a_formats() {
        let mut sector = vec![0xFF; 255];
        update_sector_crc(&mut sector).unwrap();
        assert!(set_rate(&mut sector, 1000, 6).is_err());
        assert!(set_rate(&mut sector, 1000, 5).is_ok());
    }

    #[test]
    fn encodes_led_slot_with_rgb_effect_layout_and_crc() {
        let mut sector = vec![0xFF; 255];
        update_sector_crc(&mut sector).unwrap();
        let options = EffectOptions {
            color: Some(crate::lighting::rgb::RgbColor { r: 1, g: 2, b: 3 }),
            ..Default::default()
        };
        set_led_slot(&mut sector, 2, Effect::parse("fixed").unwrap(), &options).unwrap();
        assert_eq!(
            &sector[LED_SLOTS_OFFSET + 2 * LED_SLOT_SIZE..LED_SLOTS_OFFSET + 3 * LED_SLOT_SIZE],
            &[0x01, 1, 2, 3, 2, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(led_slots(&sector).unwrap()[2].effect, "fixed");
        assert!(sector_crc_valid(&sector));
    }

    #[test]
    fn macro_codec_round_trips_bytes_across_jump_and_aligns_end() {
        let empty = Macro { steps: vec![] };
        assert_eq!(encode_macro(&empty).unwrap(), [0xFF, 0xFF]);
        let odd_before_end = Macro {
            steps: vec![MacroStep::WaitForRelease {
                wait_for_release: true,
            }],
        };
        assert_eq!(encode_macro(&odd_before_end).unwrap(), [0x01, 0xFF]);

        let value = Macro {
            steps: vec![
                MacroStep::Key {
                    key: "ctrl+shift+c".into(),
                },
                MacroStep::Delay { delay_ms: 100 },
                MacroStep::Text { text: "Hi!".into() },
                MacroStep::Consumer {
                    consumer: "volume_up".into(),
                },
                MacroStep::WaitForRelease {
                    wait_for_release: true,
                },
            ],
        };
        let ids = [2, 3, 4, 5];
        let (packed, locations) = pack_macros(&[value], &ids, 18).unwrap();
        assert!(packed.iter().any(|(_, bytes)| bytes[..16].contains(&0x60)));
        let map = packed.into_iter().collect::<BTreeMap<_, _>>();
        let decoded = decode_macro(
            &map,
            &ids.into_iter().collect(),
            locations[0].0,
            locations[0].1,
        )
        .unwrap();
        let (repacked, _) = pack_macros(&[decoded], &ids, 18).unwrap();
        assert_eq!(map, repacked.into_iter().collect());
    }

    #[test]
    fn export_import_json_round_trips_synthetic_full_sector_set() {
        let raw = [1, 4, 1, 1, 0, 5, 4, 0, 0xFF, 0, 0, 0, 0, 0, 0, 0];
        let description = Description::parse(raw).unwrap();
        let mut directory = vec![0xFF; 255];
        directory[..6].copy_from_slice(&[0, 1, 1, 0, 0xFF, 0xFF]);
        update_sector_crc(&mut directory).unwrap();

        let value = Macro {
            steps: vec![MacroStep::Key { key: "f5".into() }],
        };
        let (macro_sectors, locations) = pack_macros(&[value], &[2, 3], 255).unwrap();
        let mut profile = vec![0xFF; 255];
        profile[0] = 1;
        update_sector_crc(&mut profile).unwrap();
        set_profile_name(&mut profile, "Keyboard").unwrap();
        set_button(
            &mut profile,
            1,
            false,
            &Binding::Macro {
                sector: locations[0].0,
                offset: locations[0].1,
            },
            5,
        )
        .unwrap();
        let dump = SectorDump {
            description,
            sectors: vec![
                (0, directory),
                (1, profile),
                macro_sectors[0].clone(),
                macro_sectors[1].clone(),
            ],
        };
        let exported = export_state(&dump, "KEYBOARD").unwrap();
        let json = serde_json::to_string_pretty(&exported).unwrap();
        let decoded: OnboardExport = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, exported);
        assert!(import_plan(&decoded, &dump, "KEYBOARD").unwrap().is_empty());
    }

    fn synthetic_three_profile_keyboard() -> SectorDump {
        let raw = [1, 4, 1, 3, 3, 8, 8, 0, 0xFF, 0, 0, 0, 0, 0, 0, 0];
        let description = Description::parse(raw).unwrap();
        let mut directory = vec![0xFF; 255];
        directory[..14].copy_from_slice(&[0, 1, 1, 0xA1, 0, 2, 1, 0xA2, 0, 3, 1, 0xA3, 0xFF, 0xFF]);
        update_sector_crc(&mut directory).unwrap();
        let mut sectors = vec![(0, directory)];

        for profile_index in 1..=3 {
            let mut profile = vec![0xFF; 255];
            profile[0] = 1;
            update_sector_crc(&mut profile).unwrap();
            set_profile_name(&mut profile, &format!("Profile {profile_index}")).unwrap();
            for number in 6..=8 {
                set_button(
                    &mut profile,
                    number,
                    false,
                    &Binding::EnableProfile((number - 5) as u8),
                    8,
                )
                .unwrap();
            }
            set_button(
                &mut profile,
                1,
                true,
                &Binding::Macro {
                    sector: 4,
                    offset: ((profile_index - 1) * 2) as u16,
                },
                8,
            )
            .unwrap();
            if profile_index == 2 {
                profile[LED_SLOTS_OFFSET..LED_SLOTS_OFFSET + LED_SLOT_SIZE]
                    .copy_from_slice(&[3, 0, 0, 0, 0, 0, 8, 0x34, 0x64, 0, 0]);
                profile[LED_SLOTS_OFFSET + LED_SLOT_SIZE..LED_SLOTS_OFFSET + 2 * LED_SLOT_SIZE]
                    .copy_from_slice(&[4, 0, 0, 0, 0, 0, 0, 0x34, 1, 0x64, 8]);
                update_sector_crc(&mut profile).unwrap();
            }
            sectors.push((profile_index as u16, profile));
        }
        sectors.extend((4..8).map(|sector| (sector, vec![0xFF; 255])));
        SectorDump {
            description,
            sectors,
        }
    }

    #[test]
    fn raw_patch_round_trips_three_profiles_with_blank_dangling_macros() {
        let dump = synthetic_three_profile_keyboard();
        let exported = export_state(&dump, "KEYBOARD").unwrap();
        let json = serde_json::to_string_pretty(&exported).unwrap();
        let decoded: OnboardExport = serde_json::from_str(&json).unwrap();

        assert!(decoded.profiles.iter().all(|profile| {
            profile.bindings[5..8]
                .iter()
                .enumerate()
                .all(|(index, binding)| binding.binding == format!("enable-profile:{}", index + 1))
        }));
        assert!(decoded.macro_sectors.iter().all(|sector| {
            decode_hex(&sector.raw_hex)
                .unwrap()
                .iter()
                .all(|byte| *byte == 0xFF)
        }));
        assert!(import_plan(&decoded, &dump, "KEYBOARD").unwrap().is_empty());
    }

    #[test]
    fn raw_patch_changes_only_the_profile_with_an_edited_binding() {
        let dump = synthetic_three_profile_keyboard();
        let mut exported = export_state(&dump, "KEYBOARD").unwrap();
        exported.profiles[1].bindings[0].binding = "key:f12".into();

        let diffs = import_plan(&exported, &dump, "KEYBOARD").unwrap();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].sector, 2);
        assert_eq!(
            &diffs[0].replacement[BUTTONS_OFFSET..BUTTONS_OFFSET + 4],
            &[0x80, 2, 0, 0x45]
        );
    }

    #[test]
    fn changed_macro_uses_a_free_sector_without_moving_dangling_references() {
        let dump = synthetic_three_profile_keyboard();
        let mut exported = export_state(&dump, "KEYBOARD").unwrap();
        exported.profiles[0].gshift_bindings[0].r#macro = Some(Macro {
            steps: vec![MacroStep::Key { key: "f5".into() }],
        });

        let diffs = import_plan(&exported, &dump, "KEYBOARD").unwrap();
        assert_eq!(
            diffs.iter().map(|diff| diff.sector).collect::<Vec<_>>(),
            [1, 5]
        );
        assert_eq!(
            &diffs[0].replacement[GSHIFT_BUTTONS_OFFSET..GSHIFT_BUTTONS_OFFSET + 4],
            &[0, 5, 0, 0]
        );
        assert!(diffs.iter().all(|diff| diff.sector != 4));
    }

    #[test]
    fn directory_patch_preserves_untouched_reserved_bytes() {
        let dump = synthetic_three_profile_keyboard();
        let mut exported = export_state(&dump, "KEYBOARD").unwrap();
        exported.directory.entries[1].enabled = false;
        exported.profiles[1].enabled = false;

        let diffs = import_plan(&exported, &dump, "KEYBOARD").unwrap();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].sector, 0);
        assert_eq!(diffs[0].replacement[3], 0xA1);
        assert_eq!(diffs[0].replacement[7], 0xFF);
        assert_eq!(diffs[0].replacement[11], 0xA3);
    }

    #[test]
    fn dump_round_trips_only_directory_sectors() {
        let raw = [1, 3, 1, 1, 0, 16, 2, 0, 0xFF, 0, 0, 0, 0, 0, 0, 0];
        let description = Description::parse(raw).unwrap();
        let mut directory = vec![0xFF; 255];
        directory[..8].copy_from_slice(&[0, 1, 1, 0, 0xFF, 0xFF, 0, 0]);
        update_sector_crc(&mut directory).unwrap();
        let mut profile = vec![0xFF; 255];
        update_sector_crc(&mut profile).unwrap();
        let dump = SectorDump {
            description,
            sectors: vec![(0, directory), (1, profile)],
        };
        let bytes = encode_dump(&dump).unwrap();
        let decoded = decode_dump(&bytes).unwrap();
        assert_eq!(decoded.sectors, dump.sectors);
    }
}

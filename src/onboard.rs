use std::collections::BTreeSet;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::hidpp::device::Device;

const FEATURE_ONBOARD_PROFILES: u16 = 0x8100;
const BUTTONS_OFFSET: usize = 32;
const GSHIFT_BUTTONS_OFFSET: usize = 96;
const BUTTON_COUNT: usize = 16;
const MIN_PROFILE_SECTOR_SIZE: usize = GSHIFT_BUTTONS_OFFSET + BUTTON_COUNT * 4 + 2;
const DUMP_MAGIC: &[u8; 8] = b"BLHOB001";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
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

    fn validate(&self) -> Result<()> {
        ensure!(
            self.memory_model_id == 0x01,
            "unsupported memory model 0x{:02X}",
            self.memory_model_id
        );
        ensure!(
            matches!(self.profile_format_id, 0x01..=0x05),
            "unsupported profile format 0x{:02X}",
            self.profile_format_id
        );
        ensure!(
            self.macro_format_id == 0x01,
            "unsupported macro format 0x{:02X}",
            self.macro_format_id
        );
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

#[derive(Debug, Clone, Serialize)]
pub struct DirectoryEntry {
    pub index: usize,
    pub sector: u16,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    Mouse(u8),
    Key { modifiers: u8, usage: u8 },
    Special(u8),
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
            Self::Special(code) => Ok([0x90, *code, 0, 0]),
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
            (0x90, _) => Self::Special(raw[1]),
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
            Self::Special(code) => write!(f, "{}", special_name(*code)),
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

#[derive(Debug, Clone)]
pub struct SectorDump {
    pub description: Description,
    pub sectors: Vec<(u16, Vec<u8>)>,
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
        ensure!(
            response.len() >= 16,
            "getDescription returned only {} bytes",
            response.len()
        );
        let mut raw = [0_u8; 16];
        raw.copy_from_slice(&response[..16]);
        Description::parse(raw)
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
    ) -> Result<()> {
        ensure!(
            sector != 0 || allow_directory,
            "sector 0 may only be written by restore"
        );
        ensure!(
            expected_original.len() == replacement.len(),
            "sector length changed"
        );
        ensure!(
            sector_crc_valid(expected_original),
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

        let verified = self.read_sector(sector, size)?;
        ensure!(
            verified == replacement,
            "sector read-back differs from written data"
        );
        ensure!(
            sector_crc_valid(&verified),
            "sector read-back CRC is invalid"
        );
        Ok(())
    }

    pub fn directory(&self, description: &Description) -> Result<(Vec<u8>, Vec<DirectoryEntry>)> {
        let data = self.read_sector(0, description.sector_size)?;
        let entries = parse_directory(&data, description.profile_count)?;
        Ok((data, entries))
    }

    pub fn dump(&self) -> Result<SectorDump> {
        let description = self.description()?;
        let (directory, entries) = self.directory(&description)?;
        let mut sectors = vec![(0, directory)];
        let mut seen = BTreeSet::from([0_u16]);
        for entry in entries {
            if seen.insert(entry.sector) {
                let data = self.read_sector(entry.sector, description.sector_size)?;
                ensure!(
                    sector_crc_valid(&data),
                    "sector 0x{:04X} has an invalid CRC",
                    entry.sector
                );
                sectors.push((entry.sector, data));
            }
        }
        Ok(SectorDump {
            description,
            sectors,
        })
    }
}

pub fn parse_directory(data: &[u8], profile_count: u8) -> Result<Vec<DirectoryEntry>> {
    ensure!(sector_crc_valid(data), "profile directory CRC is invalid");
    let mut entries = Vec::new();
    for index in 0..usize::from(profile_count) {
        let offset = index * 4;
        ensure!(
            offset + 4 <= data.len() - 2,
            "profile directory is truncated"
        );
        let sector = u16::from_be_bytes([data[offset], data[offset + 1]]);
        if sector == 0xFFFF {
            break;
        }
        ensure!(
            sector != 0,
            "profile directory points a profile at sector 0"
        );
        entries.push(DirectoryEntry {
            index: index + 1,
            sector,
            enabled: data[offset + 2] != 0,
        });
    }
    Ok(entries)
}

pub fn first_enabled_sector(entries: &[DirectoryEntry]) -> Result<u16> {
    entries
        .iter()
        .find(|entry| entry.enabled)
        .map(|entry| entry.sector)
        .context("profile directory has no enabled profile")
}

pub fn button_rows(sector: &[u8]) -> Result<Vec<ButtonRow>> {
    validate_profile_sector(sector)?;
    let mut rows = Vec::with_capacity(32);
    for (gshift, base) in [(false, BUTTONS_OFFSET), (true, GSHIFT_BUTTONS_OFFSET)] {
        for index in 0..BUTTON_COUNT {
            let offset = base + index * 4;
            let raw: [u8; 4] = sector[offset..offset + 4].try_into().unwrap();
            rows.push(ButtonRow {
                button: format!("G{}", index + 1),
                gshift,
                binding: Binding::decode(raw).to_string(),
                raw: raw.iter().map(|byte| format!("{byte:02X}")).collect(),
            });
        }
    }
    Ok(rows)
}

pub fn set_button(sector: &mut [u8], number: usize, gshift: bool, binding: &Binding) -> Result<()> {
    validate_profile_sector(sector)?;
    ensure!(
        (1..=BUTTON_COUNT).contains(&number),
        "button number must be 1..=16"
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

pub fn set_rate(sector: &mut [u8], hz: u32) -> Result<()> {
    validate_profile_sector(sector)?;
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
            let mut frame = [0_u8; 16];
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
    let lower = value.to_ascii_lowercase();
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
            _ => bail!("unknown mouse button {name:?}"),
        };
        return Ok(Binding::Mouse(button));
    }
    if let Some(combo) = lower.strip_prefix("key:") {
        let (modifiers, usage) = parse_key_combo(combo)?;
        return Ok(Binding::Key { modifiers, usage });
    }
    let special = match lower.as_str() {
        "dpi-up" => 0x03,
        "dpi-down" => 0x04,
        "dpi-cycle" => 0x05,
        "dpi-shift" => 0x07,
        _ => bail!("invalid binding {value:?}"),
    };
    Ok(Binding::Special(special))
}

pub fn parse_key_combo(combo: &str) -> Result<(u8, u8)> {
    let mut modifiers = 0_u8;
    let mut usage = None;
    for part in combo.split('+') {
        ensure!(!part.is_empty(), "empty key-combo component");
        let lower = part.to_ascii_lowercase();
        let modifier = match lower.as_str() {
            "ctrl" | "control" => Some(0x01),
            "shift" => Some(0x02),
            "alt" => Some(0x04),
            "win" | "meta" | "super" => Some(0x08),
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
    Ok((
        modifiers,
        usage.context("key combo has no non-modifier key")?,
    ))
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

fn format_key_combo(modifiers: u8, usage: u8) -> String {
    let mut parts = Vec::new();
    for (mask, name) in [(1, "ctrl"), (2, "shift"), (4, "alt"), (8, "win")] {
        if modifiers & mask != 0 {
            parts.push(name.to_owned());
        }
    }
    parts.push(usage_name(usage));
    parts.join("+")
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
        0x03 => "dpi-up".into(),
        0x04 => "dpi-down".into(),
        0x05 => "dpi-cycle".into(),
        0x07 => "dpi-shift".into(),
        _ => format!("special:0x{code:02X}"),
    }
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
            sector_crc_valid(data),
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
            sector_crc_valid(&bytes),
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
    let mut expected = BTreeSet::from([0_u16]);
    expected.extend(
        parse_directory(directory, description.profile_count)?
            .into_iter()
            .map(|entry| entry.sector),
    );
    ensure!(
        seen == expected,
        "dump sector set does not match its profile directory"
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
            Binding::Special(7),
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
    fn parses_key_combos_case_insensitively() {
        assert_eq!(parse_key_combo("ctrl+c").unwrap(), (1, 0x06));
        assert_eq!(parse_key_combo("WIN+L").unwrap(), (8, 0x0F));
        assert_eq!(parse_key_combo("f11").unwrap(), (0, 0x44));
        assert_eq!(parse_key_combo("Ctrl+Shift+Esc").unwrap(), (3, 0x29));
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
    fn splits_255_byte_write_into_16_padded_frames() {
        let data = (0..255).map(|value| value as u8).collect::<Vec<_>>();
        let chunks = write_chunks(&data);
        assert_eq!(chunks.len(), 16);
        assert_eq!(&chunks[0], &data[..16]);
        assert_eq!(&chunks[15][..15], &data[240..255]);
        assert_eq!(chunks[15][15], 0);
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

use std::fmt;
use std::rc::Rc;
use std::time::{Duration, Instant};

use hidapi::HidDevice;
use thiserror::Error;

pub const REPORT_ID_SHORT: u8 = 0x10;
pub const REPORT_ID_LONG: u8 = 0x11;
pub const SHORT_LEN: usize = 7;
pub const LONG_LEN: usize = 20;
const READ_TIMEOUT: Duration = Duration::from_millis(2_000);
const READ_POLL_TIMEOUT: Duration = Duration::from_millis(25);
const MAX_PACKETS_PER_ATTEMPT: usize = 32;
const MAX_ATTEMPTS: usize = 2;

#[derive(Debug, Error)]
pub enum HidppError {
    #[error("HID I/O error: {0}")]
    Io(String),
    #[error("no {0} HID++ channel is available for this report")]
    MissingChannel(&'static str),
    #[error("device did not respond within {0} ms")]
    Timeout(u64),
    #[error("malformed HID++ packet: {0}")]
    Malformed(String),
    #[error("HID++ feature 0x{0:04X} is unsupported")]
    UnsupportedFeature(u16),
    #[error("HID++ 1.0 error {code} ({name})")]
    Hidpp10 { code: u8, name: &'static str },
    #[error("HID++ 2.0 error {code} ({name})")]
    Hidpp20 { code: u8, name: &'static str },
}

impl HidppError {
    pub fn is_protocol_error(&self) -> bool {
        matches!(self, Self::Hidpp10 { .. } | Self::Hidpp20 { .. })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Packet {
    bytes: Vec<u8>,
}

impl Packet {
    pub fn new(bytes: &[u8]) -> Result<Self, HidppError> {
        let expected = match bytes.first() {
            Some(&REPORT_ID_SHORT) => SHORT_LEN,
            Some(&REPORT_ID_LONG) => LONG_LEN,
            Some(id) => {
                return Err(HidppError::Malformed(format!(
                    "unsupported report ID 0x{id:02X}"
                )));
            }
            None => return Err(HidppError::Malformed("empty packet".into())),
        };
        if bytes.len() < expected {
            return Err(HidppError::Malformed(format!(
                "report 0x{:02X} has {} bytes, expected {expected}",
                bytes[0],
                bytes.len()
            )));
        }
        Ok(Self {
            bytes: bytes[..expected].to_vec(),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn params(&self) -> &[u8] {
        &self.bytes[4..]
    }
}

impl fmt::Debug for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Packet(")?;
        for (index, byte) in self.bytes.iter().enumerate() {
            if index > 0 {
                write!(f, " ")?;
            }
            write!(f, "{byte:02X}")?;
        }
        write!(f, ")")
    }
}

pub fn short_frame(dev_idx: u8, sub_id: u8, params: [u8; 4]) -> [u8; SHORT_LEN] {
    [
        REPORT_ID_SHORT,
        dev_idx,
        sub_id,
        params[0],
        params[1],
        params[2],
        params[3],
    ]
}

pub fn long_frame(dev_idx: u8, feature_idx: u8, fn_sw: u8, params: &[u8]) -> [u8; LONG_LEN] {
    let mut frame = [0_u8; LONG_LEN];
    frame[0] = REPORT_ID_LONG;
    frame[1] = dev_idx;
    frame[2] = feature_idx;
    frame[3] = fn_sw;
    let count = params.len().min(16);
    frame[4..4 + count].copy_from_slice(&params[..count]);
    frame
}

pub const fn fn_sw(function: u8) -> u8 {
    (function << 4) | 0x01
}

pub fn dpi_from_be(high: u8, low: u8) -> u16 {
    u16::from_be_bytes([high, low])
}

pub fn dpi_to_be(dpi: u16) -> [u8; 2] {
    dpi.to_be_bytes()
}

pub struct HidTransport {
    short: Option<Rc<HidDevice>>,
    long: Option<Rc<HidDevice>>,
}

impl HidTransport {
    pub fn with_channels(
        short: Option<HidDevice>,
        long: Option<HidDevice>,
    ) -> Result<Self, HidppError> {
        if short.is_none() && long.is_none() {
            return Err(HidppError::Malformed(
                "transport requires at least one HID channel".into(),
            ));
        }
        Ok(Self {
            short: short.map(Rc::new),
            long: long.map(Rc::new),
        })
    }

    pub fn shared(device: HidDevice) -> Self {
        let device = Rc::new(device);
        Self {
            short: Some(Rc::clone(&device)),
            long: Some(device),
        }
    }

    pub fn transact(&self, request: &[u8]) -> Result<Packet, HidppError> {
        let write_device = match (request.first(), request.len()) {
            (Some(&REPORT_ID_SHORT), SHORT_LEN) => self
                .short
                .as_deref()
                .ok_or(HidppError::MissingChannel("short"))?,
            (Some(&REPORT_ID_LONG), LONG_LEN) => self
                .long
                .as_deref()
                .ok_or(HidppError::MissingChannel("long"))?,
            (Some(report_id), length) => {
                return Err(HidppError::Malformed(format!(
                    "report 0x{report_id:02X} request has {length} bytes"
                )));
            }
            (None, _) => return Err(HidppError::Malformed("empty request".into())),
        };

        for attempt in 0..MAX_ATTEMPTS {
            write_device
                .write(request)
                .map_err(|error| HidppError::Io(error.to_string()))?;

            let deadline = Instant::now() + READ_TIMEOUT;
            let read_devices = self.read_devices();
            let mut packets_read = 0;
            while Instant::now() < deadline && packets_read < MAX_PACKETS_PER_ATTEMPT {
                for device in &read_devices {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let poll = remaining.min(READ_POLL_TIMEOUT);
                    let timeout_ms = poll.as_millis().clamp(1, i32::MAX as u128) as i32;
                    let mut buffer = [0_u8; LONG_LEN];
                    let count = device
                        .read_timeout(&mut buffer, timeout_ms)
                        .map_err(|error| HidppError::Io(error.to_string()))?;
                    if count == 0 {
                        continue;
                    }
                    packets_read += 1;

                    let Ok(packet) = Packet::new(&buffer[..count]) else {
                        continue;
                    };
                    match classify_response(request, &packet)? {
                        ResponseMatch::Matched => return Ok(packet),
                        ResponseMatch::Unrelated => continue,
                    }
                }
            }

            if attempt + 1 == MAX_ATTEMPTS {
                return Err(HidppError::Timeout(READ_TIMEOUT.as_millis() as u64));
            }
        }
        unreachable!()
    }

    fn read_devices(&self) -> Vec<&HidDevice> {
        let mut devices = Vec::with_capacity(2);
        if let Some(short) = &self.short {
            devices.push(short.as_ref());
        }
        if let Some(long) = &self.long
            && self
                .short
                .as_ref()
                .is_none_or(|short| !Rc::ptr_eq(short, long))
        {
            devices.push(long.as_ref());
        }
        devices
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseMatch {
    Matched,
    Unrelated,
}

fn classify_response(request: &[u8], response: &Packet) -> Result<ResponseMatch, HidppError> {
    let packet = response.as_bytes();
    if packet[1] != request[1] {
        return Ok(ResponseMatch::Unrelated);
    }

    // HID++ 1.0 error: offending sub-id/address are echoed in bytes 3 and 4.
    if packet[2] == 0x8F {
        if packet.len() >= SHORT_LEN && packet[3] == request[2] && packet[4] == request[3] {
            let code = packet[5];
            return Err(HidppError::Hidpp10 {
                code,
                name: hidpp10_error_name(code),
            });
        }
        return Ok(ResponseMatch::Unrelated);
    }

    // HID++ 2.0 error: feature/function are echoed after the 0xFF marker.
    if packet[0] == REPORT_ID_LONG && packet[2] == 0xFF {
        if packet[3] == request[2] && packet[4] == request[3] {
            let code = packet[5];
            return Err(HidppError::Hidpp20 {
                code,
                name: hidpp20_error_name(code),
            });
        }
        return Ok(ResponseMatch::Unrelated);
    }

    if packet[2] == request[2] && packet[3] == request[3] {
        Ok(ResponseMatch::Matched)
    } else {
        Ok(ResponseMatch::Unrelated)
    }
}

fn hidpp10_error_name(code: u8) -> &'static str {
    match code {
        1 => "invalid sub-id",
        2 => "invalid address",
        3 => "invalid value",
        4 => "connect fail",
        5 => "too many devices",
        6 => "already exists",
        7 => "busy",
        8 => "unknown device",
        9 => "resource error",
        10 => "request unavailable",
        11 => "invalid parameter value",
        12 => "wrong PIN code",
        _ => "unknown",
    }
}

fn hidpp20_error_name(code: u8) -> &'static str {
    match code {
        1 => "unknown",
        2 => "invalid argument",
        3 => "out of range",
        4 => "hardware error",
        5 => "invalid argument",
        6 => "out of range",
        7 => "unsupported",
        8 => "busy",
        9 => "unsupported",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_short_frame() {
        assert_eq!(
            short_frame(0xFF, 0x81, [0x00, 1, 2, 3]),
            [0x10, 0xFF, 0x81, 0x00, 1, 2, 3]
        );
    }

    #[test]
    fn builds_long_frame_and_zero_fills() {
        let frame = long_frame(2, 7, 0x31, &[0x12, 0x34]);
        assert_eq!(&frame[..6], &[0x11, 2, 7, 0x31, 0x12, 0x34]);
        assert!(frame[6..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn composes_function_and_software_id() {
        assert_eq!(fn_sw(0), 0x01);
        assert_eq!(fn_sw(6), 0x61);
    }

    #[test]
    fn recognizes_hidpp_10_error() {
        let request = short_frame(1, 0x81, [0xB5, 0, 0, 0]);
        let response = Packet::new(&[0x10, 1, 0x8F, 0x81, 0xB5, 7, 0]).unwrap();
        let error = classify_response(&request, &response).unwrap_err();
        assert!(matches!(error, HidppError::Hidpp10 { code: 7, .. }));
    }

    #[test]
    fn recognizes_hidpp_20_error() {
        let request = long_frame(1, 4, 0x21, &[]);
        let mut bytes = [0_u8; LONG_LEN];
        bytes[..6].copy_from_slice(&[0x11, 1, 0xFF, 4, 0x21, 6]);
        let response = Packet::new(&bytes).unwrap();
        let error = classify_response(&request, &response).unwrap_err();
        assert!(matches!(error, HidppError::Hidpp20 { code: 6, .. }));
    }

    #[test]
    fn converts_dpi_big_endian() {
        assert_eq!(dpi_from_be(0x0C, 0x80), 3200);
        assert_eq!(dpi_to_be(3200), [0x0C, 0x80]);
    }
}

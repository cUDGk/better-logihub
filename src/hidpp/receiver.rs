use std::rc::Rc;

use super::transport::{HidTransport, HidppError, short_frame};

const RECEIVER_INDEX: u8 = 0xFF;
const GET_REGISTER_LONG: u8 = 0x83;
const PAIRING_INFORMATION: u8 = 0xB5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingSlot {
    pub slot: u8,
    pub dev_idx: u8,
    pub wireless_pid: u16,
    pub name: Option<String>,
    pub name_error: Option<String>,
}

pub struct Receiver {
    transport: Rc<HidTransport>,
}

impl Receiver {
    pub fn new(transport: Rc<HidTransport>) -> Self {
        Self { transport }
    }

    pub fn read_slot(&self, slot: u8) -> Result<PairingSlot, HidppError> {
        if slot >= 6 {
            return Err(HidppError::Malformed(format!(
                "pairing slot {slot} is outside 0..5"
            )));
        }

        let info = self.read_pairing_information(0x20 + slot)?;
        let wireless_pid = parse_pairing_pid(info.params())?;
        let (name, name_error) = match self
            .read_pairing_information(0x40 + slot)
            .and_then(|packet| parse_pairing_name(packet.params()))
        {
            Ok(name) => (Some(name), None),
            Err(error) => (None, Some(error.to_string())),
        };

        Ok(PairingSlot {
            slot,
            dev_idx: slot + 1,
            wireless_pid,
            name,
            name_error,
        })
    }

    fn read_pairing_information(
        &self,
        selector: u8,
    ) -> Result<super::transport::Packet, HidppError> {
        let request = pairing_information_request(selector);
        self.transport.transact(&request)
    }
}

fn pairing_information_request(selector: u8) -> [u8; 7] {
    short_frame(
        RECEIVER_INDEX,
        GET_REGISTER_LONG,
        [PAIRING_INFORMATION, selector, 0, 0],
    )
}

pub fn parse_pairing_pid(params: &[u8]) -> Result<u16, HidppError> {
    if params.len() < 5 {
        return Err(HidppError::Malformed(format!(
            "pairing information has {} parameter bytes, expected at least 5",
            params.len()
        )));
    }
    Ok(u16::from_be_bytes([params[3], params[4]]))
}

pub fn parse_pairing_name(params: &[u8]) -> Result<String, HidppError> {
    if params.len() < 2 {
        return Err(HidppError::Malformed(
            "device name response is too short".into(),
        ));
    }
    let length = params[1] as usize;
    if params.len() < 2 + length {
        return Err(HidppError::Malformed(format!(
            "device name says {length} bytes but only {} are present",
            params.len().saturating_sub(2)
        )));
    }
    let bytes = &params[2..2 + length];
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pairing_information_pid() {
        let params = [0x20, 0x00, 0x00, 0xC5, 0x47, 0, 0, 0];
        assert_eq!(parse_pairing_pid(&params).unwrap(), 0xC547);
    }

    #[test]
    fn parses_pairing_name() {
        let params = [0x40, 5, b'M', b'X', b' ', b'1', b'0', 0, 0];
        assert_eq!(parse_pairing_name(&params).unwrap(), "MX 10");
    }

    #[test]
    fn builds_pairing_information_as_short_request() {
        assert_eq!(
            pairing_information_request(0x22),
            [0x10, 0xFF, 0x83, 0xB5, 0x22, 0, 0]
        );
    }
}

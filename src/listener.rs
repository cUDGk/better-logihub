use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;

use crate::discovery::ManagedDevice;
use crate::gkeys::{self, FEATURE_GKEYS, GKeyEvent};
use crate::hidpp::transport::{HidTransport, Packet};
use crate::mkeys::{self, FEATURE_MKEYS, FEATURE_MR, MKeyEvent};
use crate::specialkeys::{self, FEATURE_SPECIAL_KEYS, SpecialKeyEvent};

const FEATURE_WIRELESS_STATUS: u16 = 0x1D4B;
const READ_SLICE: Duration = Duration::from_millis(25);
const WATCHED_FEATURES: [u16; 5] = [
    FEATURE_GKEYS,
    FEATURE_MKEYS,
    FEATURE_MR,
    FEATURE_SPECIAL_KEYS,
    FEATURE_WIRELESS_STATUS,
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecodedEvent {
    Gkeys {
        event: GKeyEvent,
    },
    Mkeys {
        event: MKeyEvent,
    },
    Mr {
        held: bool,
    },
    SpecialKeys {
        event: SpecialKeyEvent,
    },
    WirelessStatus {
        reconfigure: bool,
        powered_on: bool,
        payload: Vec<u8>,
    },
    Unknown {
        payload: Vec<u8>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub device: usize,
    pub name: String,
    pub hid_device_index: u8,
    pub feature_id: u16,
    pub feature_index: u8,
    pub function: u8,
    pub event: DecodedEvent,
    pub raw: String,
}

#[derive(Debug, Clone)]
struct Route {
    cli_index: usize,
    name: String,
    feature_id: u16,
}

struct WatchedTransport<'a> {
    transport: Rc<HidTransport>,
    devices: Vec<&'a ManagedDevice>,
    routes: HashMap<(u8, u8), Route>,
}

pub struct Listener<'a> {
    transports: Vec<WatchedTransport<'a>>,
    next_transport: usize,
}

impl<'a> Listener<'a> {
    pub fn new(devices: &[&'a ManagedDevice]) -> (Self, Vec<String>) {
        let mut transports: Vec<WatchedTransport<'a>> = Vec::new();
        for target in devices {
            let transport = target.device.transport();
            if let Some(existing) = transports
                .iter_mut()
                .find(|entry| Rc::ptr_eq(&entry.transport, &transport))
            {
                existing.devices.push(*target);
            } else {
                transports.push(WatchedTransport {
                    transport,
                    devices: vec![*target],
                    routes: HashMap::new(),
                });
            }
        }
        let mut listener = Self {
            transports,
            next_transport: 0,
        };
        let warnings = listener.refresh_routes();
        (listener, warnings)
    }

    /// Re-resolve feature indices after a sleeping device wakes or reconnects.
    pub fn refresh_routes(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        for transport in &mut self.transports {
            for target in &transport.devices {
                for feature_id in WATCHED_FEATURES {
                    match target.device.feature_index(feature_id) {
                        Ok(Some(feature_index)) => {
                            transport.routes.insert(
                                (target.device.device_index(), feature_index),
                                Route {
                                    cli_index: target.index,
                                    name: target.name.clone(),
                                    feature_id,
                                },
                            );
                        }
                        Ok(None) => {}
                        Err(error) => warnings.push(format!(
                            "device {} ({}) route 0x{feature_id:04X}: {error}",
                            target.index, target.name
                        )),
                    }
                }
            }
        }
        warnings
    }

    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<Notification>> {
        if self.transports.is_empty() {
            return Ok(None);
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let transport_index = self.next_transport % self.transports.len();
            self.next_transport = (transport_index + 1) % self.transports.len();
            let remaining = deadline.saturating_duration_since(Instant::now());
            let packet = self.transports[transport_index]
                .transport
                .read_packet_timeout(remaining.min(READ_SLICE))?;
            let Some(packet) = packet else {
                continue;
            };
            if let Some(event) = decode_packet(&self.transports[transport_index], &packet)? {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }
}

fn decode_packet(watched: &WatchedTransport<'_>, packet: &Packet) -> Result<Option<Notification>> {
    let bytes = packet.as_bytes();
    let Some(route) = watched.routes.get(&(bytes[1], bytes[2])) else {
        return Ok(None);
    };
    // HID++ feature notifications use software-id zero. A non-zero low nibble
    // is a response belonging to this or another process, not an input event.
    if bytes[3] & 0x0F != 0 {
        return Ok(None);
    }
    let function = bytes[3] >> 4;
    let event = decode_feature_event(route.feature_id, function, packet.params())?;
    Ok(Some(Notification {
        device: route.cli_index,
        name: route.name.clone(),
        hid_device_index: bytes[1],
        feature_id: route.feature_id,
        feature_index: bytes[2],
        function,
        event,
        raw: hex_bytes(bytes),
    }))
}

fn decode_feature_event(feature_id: u16, function: u8, payload: &[u8]) -> Result<DecodedEvent> {
    Ok(match (feature_id, function) {
        (FEATURE_GKEYS, 0) => DecodedEvent::Gkeys {
            event: gkeys::decode_event(payload)?,
        },
        (FEATURE_MKEYS, 0) => DecodedEvent::Mkeys {
            event: mkeys::decode_event(payload)?,
        },
        (FEATURE_MR, 0) => DecodedEvent::Mr {
            held: payload.first().is_some_and(|value| value & 1 != 0),
        },
        (FEATURE_SPECIAL_KEYS, event_index @ 0..=4) => DecodedEvent::SpecialKeys {
            event: specialkeys::decode_event(event_index, payload)?,
        },
        (FEATURE_WIRELESS_STATUS, 0) => DecodedEvent::WirelessStatus {
            reconfigure: payload.get(1) == Some(&1),
            powered_on: payload.get(2) == Some(&1),
            payload: payload.to_vec(),
        },
        _ => DecodedEvent::Unknown {
            payload: payload.to_vec(),
        },
    })
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_feature_and_function_decoders() {
        assert!(matches!(
            decode_feature_event(FEATURE_GKEYS, 0, &[3, 0, 0, 0]).unwrap(),
            DecodedEvent::Gkeys { event } if event.held == [1, 2]
        ));
        assert!(matches!(
            decode_feature_event(FEATURE_MKEYS, 0, &[2]).unwrap(),
            DecodedEvent::Mkeys { event } if event.held == [2]
        ));
        assert!(matches!(
            decode_feature_event(FEATURE_WIRELESS_STATUS, 0, &[0, 1, 1]).unwrap(),
            DecodedEvent::WirelessStatus {
                reconfigure: true,
                powered_on: true,
                ..
            }
        ));
    }
}

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::rc::Rc;

use anyhow::{Context, Result};
use hidapi::{HidApi, HidDevice};
use serde::Serialize;

use crate::hidpp::device::Device;
use crate::hidpp::receiver::Receiver;
use crate::hidpp::transport::{HidTransport, HidppError};

const LOGITECH_VENDOR_ID: u16 = 0x046D;
const HIDPP_USAGE_PAGE: u16 = 0xFF00;
const BOLT_USAGE_PAGE: u16 = 0xFF43;
const SHORT_USAGE: u16 = 0x0001;
const LONG_USAGE: u16 = 0x0002;

#[derive(Debug, Clone, Serialize)]
pub struct ListRow {
    pub index: usize,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub wireless_pid: Option<u16>,
    pub status: String,
}

pub struct ManagedDevice {
    pub index: usize,
    pub name: String,
    pub device: Device,
}

pub struct Discovery {
    pub rows: Vec<ListRow>,
    pub devices: Vec<ManagedDevice>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PhysicalKey {
    Path(String),
    Fallback { vendor_id: u16, product_id: u16 },
}

struct PhysicalGroup {
    product_id: u16,
    product_name: Option<String>,
    short_path: Option<CString>,
    long_path: Option<CString>,
    shared_path: Option<CString>,
}

pub fn discover() -> Result<Discovery> {
    let api = HidApi::new().context("failed to initialize hidapi")?;
    let groups = collect_physical_groups(&api);
    let mut rows = Vec::new();
    let mut devices = Vec::new();
    let mut warnings = Vec::new();
    let mut next_index = 0;

    for group in groups {
        let is_receiver = is_receiver_pid(group.product_id);
        let endpoint_name = group.product_name.clone().unwrap_or_else(|| {
            if is_receiver {
                format!("Logitech receiver {:04X}", group.product_id)
            } else {
                format!("Logitech device {:04X}", group.product_id)
            }
        });

        let (transport, channel_status) =
            match open_transport(&api, &group, &endpoint_name, &mut warnings) {
                Ok(result) => result,
                Err(error) => {
                    rows.push(ListRow {
                        index: next_index,
                        kind: if is_receiver { "receiver" } else { "direct" }.into(),
                        name: endpoint_name,
                        wireless_pid: None,
                        status: format!("open failed: {error}"),
                    });
                    next_index += 1;
                    continue;
                }
            };
        let transport = Rc::new(transport);

        if is_receiver {
            rows.push(ListRow {
                index: next_index,
                kind: "receiver".into(),
                name: endpoint_name.clone(),
                wireless_pid: None,
                status: channel_status,
            });
            next_index += 1;

            let receiver = Receiver::new(Rc::clone(&transport));
            let mut slot_protocol_errors = 0;
            for slot in 0..6 {
                match receiver.read_slot(slot) {
                    Ok(pairing) => {
                        let device = Device::new(Rc::clone(&transport), pairing.dev_idx);
                        if let Some(error) = pairing.name_error {
                            warnings.push(format!(
                                "{} pairing slot {} name could not be read: {error}",
                                endpoint_name,
                                slot + 1
                            ));
                        }
                        let probe_error = device.probe().err();
                        let reachable = probe_error.is_none();
                        let name = if reachable {
                            match device.name() {
                                Ok(name) => Some(name),
                                Err(error) => {
                                    warnings.push(format!(
                                        "{} pairing slot {} HID++ name could not be read: {error}",
                                        endpoint_name,
                                        slot + 1
                                    ));
                                    pairing.name
                                }
                            }
                        } else {
                            pairing.name
                        }
                        .unwrap_or_else(|| format!("Wireless device {}", pairing.dev_idx));
                        let status = if let Some(error) = probe_error {
                            format!("unreachable: {error}")
                        } else {
                            "connected".into()
                        };
                        push_wireless_device(
                            &mut rows,
                            &mut devices,
                            &mut next_index,
                            name,
                            Some(pairing.wireless_pid),
                            status,
                            device,
                        );
                    }
                    Err(error) if error.is_protocol_error() => {
                        slot_protocol_errors += 1;
                    }
                    Err(error) => warnings.push(format!(
                        "{} pairing slot {} could not be read: {error}",
                        endpoint_name,
                        slot + 1
                    )),
                }
            }

            // Some LIGHTSPEED/Bolt receivers do not expose register 0xB5. If
            // every slot was rejected, probe the common device indices.
            if slot_protocol_errors == 6 {
                for dev_idx in 1..=2 {
                    let device = Device::new(Rc::clone(&transport), dev_idx);
                    if device.probe().is_err() {
                        continue;
                    }
                    let name = match device.name() {
                        Ok(name) => name,
                        Err(error) => {
                            warnings.push(format!(
                                "{} fallback device {} name could not be read: {error}",
                                endpoint_name, dev_idx
                            ));
                            format!("Wireless device {dev_idx}")
                        }
                    };
                    push_wireless_device(
                        &mut rows,
                        &mut devices,
                        &mut next_index,
                        name,
                        None,
                        "connected".into(),
                        device,
                    );
                }
            }
        } else {
            let device = Device::new(Rc::clone(&transport), 0xFF);
            let probe_error = device.probe().err();
            let reachable = probe_error.is_none();
            let name = if reachable {
                match device.name() {
                    Ok(name) => name,
                    Err(error) => {
                        warnings.push(format!("{endpoint_name} name could not be read: {error}"));
                        endpoint_name
                    }
                }
            } else {
                endpoint_name
            };
            rows.push(ListRow {
                index: next_index,
                kind: "direct device".into(),
                name: name.clone(),
                wireless_pid: None,
                status: if let Some(error) = probe_error {
                    format!("unreachable: {error}")
                } else {
                    channel_status
                },
            });
            devices.push(ManagedDevice {
                index: next_index,
                name,
                device,
            });
            next_index += 1;
        }
    }

    Ok(Discovery {
        rows,
        devices,
        warnings,
    })
}

fn collect_physical_groups(api: &HidApi) -> Vec<PhysicalGroup> {
    let mut groups = Vec::<PhysicalGroup>::new();
    let mut indices = HashMap::<PhysicalKey, usize>::new();

    for info in api.device_list() {
        if info.vendor_id() != LOGITECH_VENDOR_ID {
            continue;
        }
        let role = match (info.usage_page(), info.usage()) {
            (HIDPP_USAGE_PAGE, SHORT_USAGE) => ChannelRole::Short,
            (HIDPP_USAGE_PAGE, LONG_USAGE) => ChannelRole::Long,
            (BOLT_USAGE_PAGE, _) => ChannelRole::Shared,
            _ => continue,
        };

        let key = physical_device_key(info.path(), info.vendor_id(), info.product_id());
        let group_index = *indices.entry(key).or_insert_with(|| {
            let index = groups.len();
            groups.push(PhysicalGroup {
                product_id: info.product_id(),
                product_name: info.product_string().map(ToOwned::to_owned),
                short_path: None,
                long_path: None,
                shared_path: None,
            });
            index
        });
        let group = &mut groups[group_index];
        if group.product_name.is_none() {
            group.product_name = info.product_string().map(ToOwned::to_owned);
        }
        let target = match role {
            ChannelRole::Short => &mut group.short_path,
            ChannelRole::Long => &mut group.long_path,
            ChannelRole::Shared => &mut group.shared_path,
        };
        target.get_or_insert_with(|| info.path().to_owned());
    }
    groups
}

#[derive(Clone, Copy)]
enum ChannelRole {
    Short,
    Long,
    Shared,
}

fn open_transport(
    api: &HidApi,
    group: &PhysicalGroup,
    name: &str,
    warnings: &mut Vec<String>,
) -> Result<(HidTransport, String), HidppError> {
    if let Some(path) = &group.shared_path {
        return api
            .open_path(path)
            .map(HidTransport::shared)
            .map(|transport| (transport, "connected".into()))
            .map_err(|error| HidppError::Io(error.to_string()));
    }

    let short = open_channel(api, group.short_path.as_deref(), "short", name, warnings);
    let long = open_channel(api, group.long_path.as_deref(), "long", name, warnings);
    let has_short = short.is_some();
    let has_long = long.is_some();
    let status = match (has_short, has_long) {
        (true, true) => "connected".into(),
        (true, false) => "connected (long channel unavailable)".into(),
        (false, true) => "connected (short channel unavailable)".into(),
        (false, false) => {
            return Err(HidppError::Io(
                "neither short nor long HID++ channel could be opened".into(),
            ));
        }
    };
    HidTransport::with_channels(short, long).map(|transport| (transport, status))
}

fn open_channel(
    api: &HidApi,
    path: Option<&CStr>,
    role: &str,
    name: &str,
    warnings: &mut Vec<String>,
) -> Option<HidDevice> {
    let Some(path) = path else {
        warnings.push(format!("{name} has no {role} HID++ collection"));
        return None;
    };
    match api.open_path(path) {
        Ok(device) => Some(device),
        Err(error) => {
            warnings.push(format!("could not open {name} {role} channel: {error}"));
            None
        }
    }
}

fn push_wireless_device(
    rows: &mut Vec<ListRow>,
    devices: &mut Vec<ManagedDevice>,
    next_index: &mut usize,
    name: String,
    wireless_pid: Option<u16>,
    status: String,
    device: Device,
) {
    rows.push(ListRow {
        index: *next_index,
        kind: "wireless device".into(),
        name: name.clone(),
        wireless_pid,
        status,
    });
    devices.push(ManagedDevice {
        index: *next_index,
        name,
        device,
    });
    *next_index += 1;
}

fn physical_device_key(path: &CStr, vendor_id: u16, product_id: u16) -> PhysicalKey {
    normalize_physical_path(&path.to_string_lossy())
        .map(PhysicalKey::Path)
        .unwrap_or(PhysicalKey::Fallback {
            vendor_id,
            product_id,
        })
}

fn normalize_physical_path(path: &str) -> Option<String> {
    let mut segments = path
        .to_ascii_lowercase()
        .split('#')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if segments.len() < 4 {
        return None;
    }

    let collection_tokens = segments[1].split('&').collect::<Vec<_>>();
    let mut removed_collection = false;
    let hardware_id = collection_tokens
        .into_iter()
        .filter(|token| {
            let suffix = token.strip_prefix("col");
            let is_collection = suffix.is_some_and(|value| {
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
            });
            removed_collection |= is_collection;
            !is_collection
        })
        .collect::<Vec<_>>()
        .join("&");
    if !removed_collection {
        return None;
    }

    let (instance, collection_number) = segments[2].rsplit_once('&')?;
    if collection_number.len() != 4
        || !collection_number.starts_with("000")
        || !collection_number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let instance = instance.to_owned();
    segments[1] = hardware_id;
    segments[2] = instance;
    Some(segments.join("#"))
}

fn is_receiver_pid(product_id: u16) -> bool {
    matches!(
        product_id,
        0xC517
            | 0xC518
            | 0xC51A
            | 0xC521
            | 0xC525
            | 0xC526
            | 0xC52B
            | 0xC52F
            | 0xC531
            | 0xC532
            | 0xC534
            | 0xC537
            | 0xC539
            | 0xC53A
            | 0xC53F
            | 0xC541
            | 0xC542
            | 0xC545
            | 0xC547
            | 0xC548
    )
}

pub fn error_text(error: HidppError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_collection_token_and_instance_suffix() {
        let first = r"\\?\HID#VID_046D&PID_C52B&MI_02&COL01#8&2C10688F&0&0000#{4D1E55B2-F16F-11CF-88CB-001111000030}";
        let second = r"\\?\hid#vid_046d&pid_c52b&mi_02&col02#8&2c10688f&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_eq!(
            normalize_physical_path(first),
            normalize_physical_path(second)
        );
        assert_eq!(
            normalize_physical_path(first).unwrap(),
            r"\\?\hid#vid_046d&pid_c52b&mi_02#8&2c10688f&0#{4d1e55b2-f16f-11cf-88cb-001111000030}"
        );
    }

    #[test]
    fn falls_back_to_vid_pid_for_unknown_path_format() {
        let path = CString::new("unexpected-device-path").unwrap();
        assert_eq!(
            physical_device_key(&path, 0x046D, 0xC547),
            PhysicalKey::Fallback {
                vendor_id: 0x046D,
                product_id: 0xC547
            }
        );
    }
}

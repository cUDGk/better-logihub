use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

const DEVICES_JSON: &str = include_str!("../data/devices.json");

#[derive(Debug, Deserialize)]
struct Registry {
    devices: Vec<DeviceRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub model_id: String,
    pub display_name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub pids: Vec<String>,
    pub slot_prefix: Option<String>,
    pub lighting: Lighting,
    pub input: Input,
    pub gkeys: Gkeys,
    pub onboard: OnboardSupport,
    pub dpi_default: Option<DpiDefault>,
    pub per_key_map: Option<PerKeyMap>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lighting {
    pub category: Option<String>,
    pub per_key: bool,
    pub zones: Vec<LightingZone>,
    pub persistence: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightingZone {
    pub zone_type: String,
    pub effects: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub categories: Vec<String>,
    pub layers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gkeys {
    pub count: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnboardSupport {
    pub supported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DpiDefault {
    pub levels: Vec<u16>,
    pub default: u16,
    pub shift: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerKeyEntry {
    pub label: String,
    pub component: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerKeyMap {
    #[serde(default)]
    pub zone_scheme: Option<String>,
    #[serde(flatten)]
    pub entries: BTreeMap<String, PerKeyEntry>,
}

pub fn lookup(vid: u16, pid: u16) -> Option<&'static DeviceRecord> {
    if vid != 0x046D {
        return None;
    }
    registry().devices.iter().find(|device| {
        device
            .pids
            .iter()
            .filter_map(|value| value.strip_prefix("0x"))
            .any(|value| u16::from_str_radix(value, 16) == Ok(pid))
    })
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        serde_json::from_str(DEVICES_JSON).expect("embedded data/devices.json must be valid")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_all_generated_devices_and_looks_up_pid() {
        assert_eq!(registry().devices.len(), 141);
        let keyboard = lookup(0x046D, 0x407C).unwrap();
        assert_eq!(keyboard.model_id, "g915");
        assert_eq!(keyboard.gkeys.count, Some(5));
        assert!(
            keyboard
                .per_key_map
                .as_ref()
                .unwrap()
                .entries
                .contains_key("41")
        );

        let mouse = lookup(0x046D, 0xC099).unwrap();
        assert_eq!(mouse.model_id, "g502x");
        assert_eq!(mouse.dpi_default.as_ref().unwrap().default, 1600);
        assert!(lookup(0x1234, 0xC099).is_none());
        assert!(lookup(0x046D, 0xFFFF).is_none());
    }

    #[test]
    fn per_key_map_can_declare_a_wire_zone_scheme() {
        let map: PerKeyMap = serde_json::from_str(
            r#"{"zone_scheme":"hidusage","4":{"label":"A","component":"PERKEY_KEYBOARD_04"}}"#,
        )
        .unwrap();
        assert_eq!(map.zone_scheme.as_deref(), Some("hidusage"));
        assert!(map.entries.contains_key("4"));
    }
}

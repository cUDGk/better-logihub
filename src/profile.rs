use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};

const STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub source: String,
    pub dpi_levels: Vec<u16>,
    pub active_dpi: u16,
    pub shift_dpi: u16,
    pub report_rate_hz: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileStore {
    pub version: u32,
    pub profiles: Vec<Profile>,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            profiles: Vec::new(),
        }
    }
}

pub fn default_ghub_db_path() -> Result<PathBuf> {
    let base = env::var_os("LOCALAPPDATA")
        .context("LOCALAPPDATA is not set; pass the G HUB database with --db <path>")?;
    Ok(PathBuf::from(base).join("LGHUB").join("settings.db"))
}

pub fn default_store_path() -> Result<PathBuf> {
    let base = env::var_os("APPDATA").context("APPDATA is not set")?;
    Ok(PathBuf::from(base)
        .join("better-logihub")
        .join("profiles.json"))
}

pub fn import_ghub_database(path: &Path) -> Result<Vec<Profile>> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open G HUB settings database {}", path.display()))?;
    let blob = connection
        .query_row(
            "SELECT file FROM data ORDER BY _id DESC LIMIT 1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .context("failed to read the newest row from G HUB data table")?
        .context("G HUB data table is empty")?;
    let json = String::from_utf8(blob).context("G HUB settings BLOB is not valid UTF-8")?;
    extract_ghub_profiles(&json).context("failed to parse G HUB settings JSON")
}

pub fn load_store(path: &Path) -> Result<ProfileStore> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProfileStore::default());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read profile store {}", path.display()));
        }
    };
    let store: ProfileStore = serde_json::from_slice(&contents)
        .with_context(|| format!("failed to parse profile store {}", path.display()))?;
    if store.version != STORE_VERSION {
        bail!(
            "unsupported profile store version {} in {} (expected {STORE_VERSION})",
            store.version,
            path.display()
        );
    }
    Ok(store)
}

pub fn save_store(path: &Path, store: &ProfileStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create profile directory {}", parent.display()))?;
    }
    let mut json = serde_json::to_vec_pretty(store).context("failed to serialize profiles")?;
    json.push(b'\n');
    fs::write(path, json)
        .with_context(|| format!("failed to write profile store {}", path.display()))
}

pub fn merge_profiles(store: &mut ProfileStore, imported: &[Profile]) {
    for profile in imported {
        if let Some(existing) = store
            .profiles
            .iter_mut()
            .find(|existing| existing.name == profile.name)
        {
            *existing = profile.clone();
        } else {
            store.profiles.push(profile.clone());
        }
    }
}

fn extract_ghub_profiles(json: &str) -> Result<Vec<Profile>> {
    let document: GhubDocument = serde_json::from_str(json)?;
    let cards = document
        .cards
        .cards
        .into_iter()
        .filter(|card| card.attribute == "MOUSE_SETTINGS")
        .filter_map(|card| card.mouse_settings.map(|settings| (card.id, settings)))
        .collect::<HashMap<_, _>>();
    let applications = document
        .applications
        .applications
        .into_iter()
        .map(|application| (application.application_id, application.name))
        .collect::<HashMap<_, _>>();

    let mut profiles = Vec::new();
    for ghub_profile in document.profiles.profiles {
        let Some(card_id) = ghub_profile
            .assignments
            .iter()
            .find(|assignment| assignment.slot_id.ends_with("_mouse_settings"))
            .map(|assignment| assignment.card_id.as_str())
        else {
            continue;
        };
        let Some(settings) = cards.get(card_id) else {
            continue;
        };
        let name = if ghub_profile.name == "PROFILE_NAME_DEFAULT" {
            applications
                .get(&ghub_profile.application_id)
                .map(|name| display_application_name(name))
                .unwrap_or_else(|| ghub_profile.application_id.clone())
        } else {
            ghub_profile.name
        };
        profiles.push(Profile {
            name,
            source: "ghub-import".into(),
            dpi_levels: settings.dpi_table.levels.clone(),
            active_dpi: settings.dpi_table.active_dpi,
            shift_dpi: settings.dpi_table.shift_dpi,
            report_rate_hz: settings.report_rate.value,
        });
    }
    Ok(profiles)
}

fn display_application_name(name: &str) -> String {
    if name == "APPLICATION_NAME_DESKTOP" {
        "Desktop".into()
    } else {
        name.to_owned()
    }
}

#[derive(Deserialize)]
struct GhubDocument {
    profiles: GhubProfiles,
    cards: GhubCards,
    applications: GhubApplications,
}

#[derive(Deserialize)]
struct GhubProfiles {
    profiles: Vec<GhubProfile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhubProfile {
    name: String,
    application_id: String,
    assignments: Vec<GhubAssignment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhubAssignment {
    card_id: String,
    slot_id: String,
}

#[derive(Deserialize)]
struct GhubCards {
    cards: Vec<GhubCard>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhubCard {
    attribute: String,
    id: String,
    mouse_settings: Option<GhubMouseSettings>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhubMouseSettings {
    dpi_table: GhubDpiTable,
    report_rate: GhubReportRate,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhubDpiTable {
    active_dpi: u16,
    shift_dpi: u16,
    levels: Vec<u16>,
}

#[derive(Deserialize)]
struct GhubReportRate {
    value: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhubApplications {
    applications: Vec<GhubApplication>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhubApplication {
    application_id: String,
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_mouse_profiles_and_resolves_default_name() {
        let json = r#"
        {
          "profiles": {"profiles": [
            {
              "id": "profile-desktop",
              "name": "PROFILE_NAME_DEFAULT",
              "applicationId": "desktop-app",
              "assignments": [
                {"cardId": "buttons", "slotId": "primary_button_settings"},
                {"cardId": "mouse-card", "slotId": "primary_mouse_settings"}
              ]
            },
            {
              "id": "buttons-only",
              "name": "Buttons only",
              "applicationId": "desktop-app",
              "assignments": [
                {"cardId": "buttons", "slotId": "primary_button_settings"}
              ]
            },
            {
              "id": "missing-card",
              "name": "Missing card",
              "applicationId": "game-app",
              "assignments": [
                {"cardId": "not-present", "slotId": "game_mouse_settings"}
              ]
            }
          ]},
          "cards": {"cards": [
            {
              "attribute": "MOUSE_SETTINGS",
              "id": "mouse-card",
              "mouseSettings": {
                "dpiTable": {
                  "activeDpi": 4000,
                  "defaultDpi": 1600,
                  "shiftDpi": 800,
                  "levels": [800, 1200, 1600, 4000, 7000]
                },
                "reportRate": {"value": 1000}
              }
            },
            {"attribute": "BUTTON_SETTINGS", "id": "buttons"}
          ]},
          "applications": {"applications": [
            {"applicationId": "desktop-app", "name": "APPLICATION_NAME_DESKTOP"},
            {"applicationId": "game-app", "name": "Example Game"}
          ]}
        }
        "#;

        let profiles = extract_ghub_profiles(json).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "Desktop");
        assert_eq!(profiles[0].dpi_levels, [800, 1200, 1600, 4000, 7000]);
        assert_eq!(profiles[0].active_dpi, 4000);
        assert_eq!(profiles[0].shift_dpi, 800);
        assert_eq!(profiles[0].report_rate_hz, 1000);
    }

    #[test]
    fn merge_overwrites_same_name_and_keeps_other_profiles() {
        let mut store = ProfileStore {
            version: 1,
            profiles: vec![profile("Desktop", 800), profile("Existing Game", 1600)],
        };
        let imported = vec![profile("Desktop", 4000), profile("New Game", 3200)];

        merge_profiles(&mut store, &imported);

        assert_eq!(store.profiles.len(), 3);
        assert_eq!(store.profiles[0], profile("Desktop", 4000));
        assert_eq!(store.profiles[1], profile("Existing Game", 1600));
        assert_eq!(store.profiles[2], profile("New Game", 3200));
    }

    fn profile(name: &str, active_dpi: u16) -> Profile {
        Profile {
            name: name.into(),
            source: "ghub-import".into(),
            dpi_levels: vec![active_dpi],
            active_dpi,
            shift_dpi: active_dpi,
            report_rate_hz: 1000,
        }
    }
}

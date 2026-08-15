use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bindings::{
    self, Action, Bindings, DelayStep, DeviceBindings, KeysAction, MacroAction,
    MacroStep as DaemonMacroStep, RunAction, TextAction,
};
use crate::device_data;
use crate::lighting::rgb::Effect;
use crate::onboard::{Macro as OnboardMacro, MacroStep as OnboardMacroStep};
use crate::profile::{
    ImportedMacro, PortableOnboardProfile, Profile, ProfileBinding, RgbPreset, RgbPresetFile,
    load_store, merge_profiles, save_json, save_store,
};

const BUILTIN_PREFIX: &str = "0f82f693-5b78-4cf5-867e-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportWarning {
    pub class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportSummary {
    pub database: String,
    pub profiles_found: usize,
    pub profiles_imported: usize,
    pub cards: usize,
    pub applications: usize,
    pub assignments: usize,
    pub macro_cards: usize,
    pub lighting_cards: usize,
    pub device_models: Vec<String>,
    pub output_files: Vec<String>,
    pub dry_run: bool,
    pub unmapped_classes: BTreeMap<String, usize>,
    pub warnings: Vec<ImportWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportResult {
    pub profiles: Vec<Profile>,
    pub bindings: Bindings,
    pub summary: ImportSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedSlotId {
    pub slot_prefix: String,
    pub input: String,
    pub mode: Option<u8>,
    pub shifted: bool,
    pub attribute: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuiltinCard {
    Keystroke { usage: u8, modifiers: u8 },
    MouseButton { button: u8 },
    MouseAction { action: u8 },
    DeviceAction { action: u8 },
    Unknown { kind: u8, arg1: u8, arg2: u8 },
}

#[derive(Debug, Clone)]
struct ConvertedAction {
    source_action: String,
    daemon_action: Option<Action>,
    onboard_binding: String,
    onboard_macro: Option<OnboardMacro>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct MacroConversion {
    macro_type: String,
    daemon_action: Option<Action>,
    onboard_macro: Option<OnboardMacro>,
    warnings: Vec<String>,
}

pub fn default_ghub_db_path() -> Result<PathBuf> {
    let base = env::var_os("LOCALAPPDATA")
        .context("LOCALAPPDATA is not set; pass the G HUB database with --db <path>")?;
    Ok(PathBuf::from(base).join("LGHUB").join("settings.db"))
}

pub fn import_ghub_database(path: &Path, device_model: Option<&str>) -> Result<ImportResult> {
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
    let mut result = import_ghub_json(&json, device_model)?;
    result.summary.database = path.display().to_string();
    Ok(result)
}

pub fn import_ghub_json(json: &str, device_model: Option<&str>) -> Result<ImportResult> {
    let document: Value = serde_json::from_str(json).context("G HUB settings JSON is invalid")?;
    let profiles_json = nested_array(&document, "profiles", "profiles")?;
    let cards_json = nested_array(&document, "cards", "cards")?;
    let applications_json = nested_array(&document, "applications", "applications")?;
    let cards = cards_json
        .iter()
        .filter_map(|card| Some((card.get("id")?.as_str()?.to_owned(), card)))
        .collect::<HashMap<_, _>>();
    let applications = applications_json
        .iter()
        .filter_map(|application| {
            Some((
                application.get("applicationId")?.as_str()?.to_owned(),
                application.get("name")?.as_str()?.to_owned(),
            ))
        })
        .collect::<HashMap<_, _>>();
    let requested_model = device_model.map(resolve_requested_model).transpose()?;
    let mut warnings = Vec::new();
    let mut profiles = Vec::new();

    for raw_profile in profiles_json {
        let id = string(raw_profile, "id").unwrap_or_default();
        let application_id = string(raw_profile, "applicationId").unwrap_or_default();
        let raw_name = string(raw_profile, "name").unwrap_or_else(|| id.clone());
        let name = if raw_name == "PROFILE_NAME_DEFAULT" {
            applications
                .get(&application_id)
                .map(|name| display_application_name(name))
                .unwrap_or_else(|| application_id.clone())
        } else {
            raw_name
        };
        let assignments = raw_profile
            .get("assignments")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut imported = Profile {
            name: name.clone(),
            source: "ghub-import".into(),
            id,
            application_id,
            device_models: Vec::new(),
            dpi_levels: Vec::new(),
            default_dpi: 0,
            active_dpi: 0,
            shift_dpi: 0,
            report_rate_hz: 0,
            bindings: Vec::new(),
            macros: Vec::new(),
            lighting: Vec::new(),
            warnings: Vec::new(),
        };
        let mut referenced_macros = BTreeMap::<String, ImportedMacro>::new();
        let mut lighting_ids = BTreeSet::new();

        for field in ["lightingCard", "syncLightingCard"] {
            if let Some(id) = raw_profile.get(field).and_then(Value::as_str) {
                lighting_ids.insert(id.to_owned());
            }
        }

        for assignment in assignments {
            let Some(card_id) = assignment.get("cardId").and_then(Value::as_str) else {
                push_warning(
                    &mut warnings,
                    "invalid_assignment",
                    Some(&name),
                    None,
                    "assignment has no cardId",
                );
                continue;
            };
            let Some(slot_id) = assignment.get("slotId").and_then(Value::as_str) else {
                push_warning(
                    &mut warnings,
                    "invalid_assignment",
                    Some(&name),
                    None,
                    "assignment has no slotId",
                );
                continue;
            };
            let card = cards.get(card_id).copied();
            let attribute = card
                .and_then(|card| card.get("attribute"))
                .and_then(Value::as_str)
                .unwrap_or_default();

            if attribute == "MOUSE_SETTINGS" || slot_id.ends_with("_mouse_settings") {
                let parsed = parse_slot_id(slot_id)?;
                let model = resolve_slot_model(&parsed.slot_prefix);
                if model_selected(&model, requested_model.as_deref()) {
                    if let Some(card) = card {
                        import_mouse_settings(card, &mut imported, &mut warnings, &name, slot_id)?;
                        insert_model(&mut imported.device_models, model);
                    } else {
                        push_warning(
                            &mut warnings,
                            "missing_card",
                            Some(&name),
                            Some(slot_id),
                            format!("mouse settings card {card_id} is not present in cards[]"),
                        );
                    }
                }
                continue;
            }

            if is_lighting_attribute(attribute) {
                lighting_ids.insert(card_id.to_owned());
                continue;
            }

            if matches!(attribute, "INPUT_CONFIGURATION" | "INPUT_PRESET") {
                if let Some(card) = card {
                    import_input_configuration(
                        card,
                        slot_id,
                        &cards,
                        requested_model.as_deref(),
                        &mut imported,
                        &mut referenced_macros,
                        &mut warnings,
                    )?;
                }
                continue;
            }

            if !slot_id_contains_mode(slot_id) {
                continue;
            }
            let parsed = match parse_slot_id(slot_id) {
                Ok(parsed) => parsed,
                Err(error) => {
                    push_warning(
                        &mut warnings,
                        "invalid_slot_id",
                        Some(&name),
                        Some(slot_id),
                        error.to_string(),
                    );
                    continue;
                }
            };
            let model = resolve_slot_model(&parsed.slot_prefix);
            if !model_selected(&model, requested_model.as_deref()) {
                continue;
            }
            let conversion = convert_card(card_id, card)?;
            for reason in &conversion.warnings {
                push_warning(
                    &mut warnings,
                    warning_class(reason),
                    Some(&name),
                    Some(slot_id),
                    reason,
                );
            }
            if let Some(card) = card
                && attribute == "MACRO_PLAYBACK"
            {
                let macro_value = card.get("macro").unwrap_or(&Value::Null);
                let macro_conversion = convert_macro(macro_value)?;
                referenced_macros
                    .entry(card_id.to_owned())
                    .or_insert_with(|| ImportedMacro {
                        card_id: card_id.into(),
                        name: string(card, "name").unwrap_or_default(),
                        macro_type: macro_conversion.macro_type,
                        daemon_action: macro_conversion.daemon_action,
                        onboard_macro: macro_conversion.onboard_macro,
                        warnings: macro_conversion.warnings,
                    });
            }
            insert_model(&mut imported.device_models, model.clone());
            imported.bindings.push(ProfileBinding {
                slot_id: slot_id.into(),
                device_model: model,
                slot_prefix: parsed.slot_prefix,
                input: parsed.input,
                mode: parsed.mode.unwrap_or(1),
                shifted: parsed.shifted,
                attribute: parsed.attribute,
                card_id: card_id.into(),
                source_action: conversion.source_action,
                daemon_action: conversion.daemon_action,
                onboard_binding: conversion.onboard_binding,
                onboard_macro: conversion.onboard_macro,
                warnings: conversion.warnings,
            });
        }

        for lighting_id in lighting_ids {
            let Some(card) = cards.get(&lighting_id).copied() else {
                push_warning(
                    &mut warnings,
                    "missing_card",
                    Some(&name),
                    None,
                    format!("lighting card {lighting_id} is not present in cards[]"),
                );
                continue;
            };
            match lighting_presets(card) {
                Ok(presets) => {
                    for preset in presets {
                        if let Err(error) = Effect::parse(&preset.effect) {
                            push_warning(
                                &mut warnings,
                                "lighting_unsupported",
                                Some(&name),
                                None,
                                error.to_string(),
                            );
                            continue;
                        }
                        if preset.effect == "streaming" {
                            push_warning(
                                &mut warnings,
                                "lighting_onboard_unsupported",
                                Some(&name),
                                None,
                                "STREAMING can be applied live but is rejected by onboard LED slots",
                            );
                        }
                        imported.lighting.push(preset);
                    }
                }
                Err(error) => push_warning(
                    &mut warnings,
                    "lighting_unsupported",
                    Some(&name),
                    None,
                    error.to_string(),
                ),
            }
        }
        imported
            .bindings
            .sort_by(|left, right| left.slot_id.cmp(&right.slot_id));
        imported.macros = referenced_macros.into_values().collect();
        imported.device_models.sort();
        imported.device_models.dedup();
        if requested_model.is_none()
            || imported
                .device_models
                .iter()
                .any(|model| requested_model.as_deref() == Some(model.as_str()))
        {
            profiles.push(imported);
        }
    }

    add_unassigned_slots(&mut profiles, &mut warnings);
    profiles.sort_by(|left, right| (&left.name, &left.id).cmp(&(&right.name, &right.id)));
    warnings.sort_by(|left, right| {
        (&left.class, &left.profile, &left.slot_id, &left.reason).cmp(&(
            &right.class,
            &right.profile,
            &right.slot_id,
            &right.reason,
        ))
    });
    warnings.dedup();
    let mut bindings = build_daemon_bindings(&profiles, &mut warnings)?;
    bindings.validate()?;
    warnings.sort_by(|left, right| {
        (&left.class, &left.profile, &left.slot_id, &left.reason).cmp(&(
            &right.class,
            &right.profile,
            &right.slot_id,
            &right.reason,
        ))
    });
    warnings.dedup();
    for profile in &mut profiles {
        profile.warnings = warnings
            .iter()
            .filter(|warning| warning.profile.as_deref() == Some(profile.name.as_str()))
            .map(|warning| warning.reason.clone())
            .collect();
        profile.warnings.sort();
        profile.warnings.dedup();
    }
    let mut unmapped_classes = BTreeMap::new();
    for warning in &warnings {
        *unmapped_classes.entry(warning.class.clone()).or_insert(0) += 1;
    }
    let device_models = profiles
        .iter()
        .flat_map(|profile| profile.device_models.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let macro_cards = cards_json
        .iter()
        .filter(|card| card.get("attribute").and_then(Value::as_str) == Some("MACRO_PLAYBACK"))
        .count();
    let lighting_cards = cards_json
        .iter()
        .filter(|card| {
            card.get("attribute")
                .and_then(Value::as_str)
                .is_some_and(is_lighting_attribute)
        })
        .count();
    let summary = ImportSummary {
        database: "<json>".into(),
        profiles_found: profiles_json.len(),
        profiles_imported: profiles.len(),
        cards: cards_json.len(),
        applications: applications_json.len(),
        assignments: profiles_json
            .iter()
            .filter_map(|profile| profile.get("assignments").and_then(Value::as_array))
            .map(Vec::len)
            .sum(),
        macro_cards,
        lighting_cards,
        device_models,
        output_files: Vec::new(),
        dry_run: false,
        unmapped_classes,
        warnings,
    };
    Ok(ImportResult {
        profiles,
        bindings: std::mem::take(&mut bindings),
        summary,
    })
}

pub fn output_paths(result: &ImportResult, out_dir: &Path) -> Vec<PathBuf> {
    let mut paths = vec![out_dir.join("profiles.json"), out_dir.join("bindings.json")];
    for profile in &result.profiles {
        for model in &profile.device_models {
            let stem = format!("{}--{}", safe_name(&profile.name), safe_name(model));
            if device_data::lookup_model(model).is_some_and(|device| device.onboard.supported) {
                paths.push(out_dir.join("onboard").join(format!("{stem}.json")));
            }
            if !profile.lighting.is_empty() {
                paths.push(out_dir.join("rgb").join(format!("{stem}.json")));
            }
        }
    }
    paths
}

pub fn save_import(result: &ImportResult, out_dir: &Path) -> Result<Vec<PathBuf>> {
    let profile_path = out_dir.join("profiles.json");
    let mut store = load_store(&profile_path)?;
    merge_profiles(&mut store, &result.profiles);
    save_store(&profile_path, &store)?;

    let bindings_path = out_dir.join("bindings.json");
    let mut existing = bindings::load(&bindings_path)?;
    for (device, imported) in &result.bindings.devices {
        existing.devices.insert(device.clone(), imported.clone());
    }
    bindings::save(&bindings_path, &existing)?;

    for profile in &result.profiles {
        for model in &profile.device_models {
            let stem = format!("{}--{}", safe_name(&profile.name), safe_name(model));
            if device_data::lookup_model(model).is_some_and(|device| device.onboard.supported) {
                let portable = PortableOnboardProfile::new(model.clone(), profile.clone());
                save_json(
                    &out_dir.join("onboard").join(format!("{stem}.json")),
                    &portable,
                    "portable onboard profile",
                )?;
            }
            if !profile.lighting.is_empty() {
                save_json(
                    &out_dir.join("rgb").join(format!("{stem}.json")),
                    &RgbPresetFile::new(profile, model),
                    "RGB preset",
                )?;
            }
        }
    }
    Ok(output_paths(result, out_dir))
}

pub fn decode_builtin_card_id(id: &str) -> Result<Option<BuiltinCard>> {
    let Some(suffix) = id.strip_prefix(BUILTIN_PREFIX) else {
        return Ok(None);
    };
    ensure!(
        suffix.len() == 12 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "built-in card id suffix must contain 12 hexadecimal digits"
    );
    let bytes = (0..6)
        .map(|index| u8::from_str_radix(&suffix[index * 2..index * 2 + 2], 16))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        bytes[3..] == [0, 0, 0],
        "built-in card id has non-zero padding"
    );
    Ok(Some(match bytes[0] {
        0x01 => BuiltinCard::Keystroke {
            usage: bytes[1],
            modifiers: bytes[2],
        },
        0x02 => BuiltinCard::MouseButton { button: bytes[1] },
        0x04 => BuiltinCard::MouseAction { action: bytes[1] },
        0x09 => BuiltinCard::DeviceAction { action: bytes[1] },
        kind => BuiltinCard::Unknown {
            kind,
            arg1: bytes[1],
            arg2: bytes[2],
        },
    }))
}

pub fn parse_slot_id(value: &str) -> Result<ParsedSlotId> {
    let (base, attribute) = value
        .rsplit_once(':')
        .map_or((value, None), |(base, attribute)| {
            (base, Some(attribute.to_owned()))
        });
    let (base, shifted) = base
        .strip_suffix("_shifted")
        .map_or((base, false), |base| (base, true));
    if let Some(mode_position) = base.rfind("_m") {
        let mode_text = &base[mode_position + 2..];
        if !mode_text.is_empty() && mode_text.bytes().all(|byte| byte.is_ascii_digit()) {
            let mode = mode_text.parse::<u8>().context("slot mode is outside u8")?;
            ensure!(mode != 0, "slot mode must be 1 or greater");
            let target = &base[..mode_position];
            let (slot_prefix, input) = target
                .rsplit_once('_')
                .context("input slot has no slot-prefix separator")?;
            ensure!(!slot_prefix.is_empty(), "slot prefix is empty");
            ensure!(!input.is_empty(), "slot input is empty");
            return Ok(ParsedSlotId {
                slot_prefix: slot_prefix.into(),
                input: input.into(),
                mode: Some(mode),
                shifted,
                attribute,
            });
        }
    }
    ensure!(!shifted, "settings slot cannot have a shifted suffix");
    let suffixes = [
        "mouse_settings",
        "mouse_settings_advanced",
        "mouse_sensitivity",
        "lighting_settings",
        "lighting_setting_firmware",
        "lighting_setting_software",
        "lighting_setting_sync",
        "input_configuration",
        "disable_keys",
        "disable_controls",
        "report_rate",
    ];
    for suffix in suffixes {
        if let Some(prefix) = base.strip_suffix(&format!("_{suffix}")) {
            ensure!(!prefix.is_empty(), "settings slot prefix is empty");
            return Ok(ParsedSlotId {
                slot_prefix: prefix.into(),
                input: suffix.into(),
                mode: None,
                shifted: false,
                attribute,
            });
        }
    }
    if let Some((prefix, input)) = base.rsplit_once('_') {
        return Ok(ParsedSlotId {
            slot_prefix: prefix.into(),
            input: input.into(),
            mode: None,
            shifted: false,
            attribute,
        });
    }
    bail!("unrecognized slotId {value:?}")
}

pub fn dpi_indices(levels: &[u16], default: u16, shift: u16) -> Result<(u8, Option<u8>)> {
    ensure!(!levels.is_empty(), "DPI table has no levels");
    let default_index = levels
        .iter()
        .position(|value| *value == default)
        .with_context(|| format!("default DPI {default} is absent from levels"))?;
    let shift_index = if shift == 0 {
        None
    } else {
        Some(
            levels
                .iter()
                .position(|value| *value == shift)
                .with_context(|| format!("shift DPI {shift} is absent from levels"))?
                as u8,
        )
    };
    Ok((default_index as u8, shift_index))
}

fn nested_array<'a>(document: &'a Value, container: &str, field: &str) -> Result<&'a [Value]> {
    document
        .get(container)
        .and_then(|value| value.get(field))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .with_context(|| format!("G HUB JSON has no {container}.{field} array"))
}

fn import_mouse_settings(
    card: &Value,
    profile: &mut Profile,
    warnings: &mut Vec<ImportWarning>,
    profile_name: &str,
    slot_id: &str,
) -> Result<()> {
    let settings = card
        .get("mouseSettings")
        .context("MOUSE_SETTINGS card has no mouseSettings")?;
    let table = settings
        .get("dpiTable")
        .context("mouseSettings has no dpiTable")?;
    let levels = table
        .get("levels")
        .and_then(Value::as_array)
        .context("dpiTable has no levels")?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| u16::try_from(value).ok())
                .context("DPI level is outside u16")
        })
        .collect::<Result<Vec<_>>>()?;
    let default = u16_field(table, "defaultDpi")?;
    let active = u16_field(table, "activeDpi")?;
    let shift = u16_field(table, "shiftDpi")?;
    if let Err(error) = dpi_indices(&levels, default, shift) {
        push_warning(
            warnings,
            "dpi_value_missing",
            Some(profile_name),
            Some(slot_id),
            error.to_string(),
        );
    }
    profile.dpi_levels = levels;
    profile.default_dpi = default;
    profile.active_dpi = active;
    profile.shift_dpi = shift;
    profile.report_rate_hz = settings
        .get("reportRate")
        .and_then(|value| value.get("value"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn import_input_configuration(
    card: &Value,
    settings_slot_id: &str,
    cards: &HashMap<String, &Value>,
    requested_model: Option<&str>,
    profile: &mut Profile,
    referenced_macros: &mut BTreeMap<String, ImportedMacro>,
    warnings: &mut Vec<ImportWarning>,
) -> Result<()> {
    let parsed_settings = parse_slot_id(settings_slot_id)?;
    let model = resolve_slot_model(&parsed_settings.slot_prefix);
    if !model_selected(&model, requested_model) {
        return Ok(());
    }
    let mut recognized = false;
    let configuration = card.get("inputConfiguration").or_else(|| {
        card.get("inputPreset")
            .and_then(|value| value.get("configuration"))
    });

    if let Some(layer_map) = configuration
        .and_then(|value| value.get("layerMap"))
        .and_then(Value::as_object)
    {
        recognized = true;
        for (layer, layer_value) in layer_map {
            let Ok((mode, shifted)) = parse_layer(layer) else {
                push_warning(
                    warnings,
                    "input_layer_unsupported",
                    Some(&profile.name),
                    Some(settings_slot_id),
                    format!("input layer {layer} cannot be represented"),
                );
                continue;
            };
            let Some(assignments) = find_object(layer_value, "assignments") else {
                continue;
            };
            import_input_categories(
                assignments,
                mode,
                shifted,
                &model,
                &parsed_settings.slot_prefix,
                settings_slot_id,
                cards,
                profile,
                referenced_macros,
                warnings,
            )?;
        }
    }

    if let Some(configuration) = card.get("inputConfiguration") {
        if let Some(layer_presets) = configuration.get("layerPresets").and_then(Value::as_object) {
            recognized = true;
            for (layer, preset_id) in layer_presets {
                let Ok((mode, shifted)) = parse_layer(layer) else {
                    push_warning(
                        warnings,
                        "input_layer_unsupported",
                        Some(&profile.name),
                        Some(settings_slot_id),
                        format!("input layer {layer} cannot be represented"),
                    );
                    continue;
                };
                let Some(preset_id) = preset_id.as_str() else {
                    continue;
                };
                let Some(assignments) = cards
                    .get(preset_id)
                    .and_then(|preset| preset.get("inputPreset"))
                    .and_then(|preset| preset.get("assignments"))
                    .and_then(Value::as_object)
                else {
                    push_warning(
                        warnings,
                        "default_assignment_missing",
                        Some(&profile.name),
                        Some(settings_slot_id),
                        format!(
                            "input preset card {preset_id} is absent; device-depot defaults are not in settings.db"
                        ),
                    );
                    continue;
                };
                import_input_categories(
                    assignments,
                    mode,
                    shifted,
                    &model,
                    &parsed_settings.slot_prefix,
                    settings_slot_id,
                    cards,
                    profile,
                    referenced_macros,
                    warnings,
                )?;
            }
        }
        if let Some(assignments) = configuration
            .get("panAssignments")
            .and_then(Value::as_object)
        {
            recognized = true;
            import_input_categories(
                assignments,
                parsed_settings.mode.unwrap_or(1),
                parsed_settings.shifted,
                &model,
                &parsed_settings.slot_prefix,
                settings_slot_id,
                cards,
                profile,
                referenced_macros,
                warnings,
            )?;
        }
    }

    if let Some(assignments) = card
        .get("inputPreset")
        .and_then(|preset| preset.get("assignments"))
        .and_then(Value::as_object)
    {
        recognized = true;
        import_input_categories(
            assignments,
            parsed_settings.mode.unwrap_or(1),
            parsed_settings.shifted,
            &model,
            &parsed_settings.slot_prefix,
            settings_slot_id,
            cards,
            profile,
            referenced_macros,
            warnings,
        )?;
    }

    if !recognized {
        push_warning(
            warnings,
            "input_configuration_unsupported",
            Some(&profile.name),
            Some(settings_slot_id),
            "input card has no layerMap, layerPresets, panAssignments, or assignments",
        );
    }
    insert_model(&mut profile.device_models, model);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn import_input_categories(
    categories: &serde_json::Map<String, Value>,
    mode: u8,
    shifted: bool,
    model: &str,
    slot_prefix: &str,
    settings_slot_id: &str,
    cards: &HashMap<String, &Value>,
    profile: &mut Profile,
    referenced_macros: &mut BTreeMap<String, ImportedMacro>,
    warnings: &mut Vec<ImportWarning>,
) -> Result<()> {
    for category in categories.values() {
        let Some(input_assignments) = category.get("inputAssignments").and_then(Value::as_object)
        else {
            continue;
        };
        for (input_key, input_assignment) in input_assignments {
            let input_id = input_assignment
                .get("inputId")
                .and_then(Value::as_u64)
                .or_else(|| input_key.parse::<u64>().ok())
                .and_then(|value| u8::try_from(value).ok())
                .context("input configuration has an invalid inputId")?;
            let exclusive = exclusive_assignment(input_assignment);
            let (card_id, conversion) = match exclusive.as_str() {
                "GSHIFT" => ("exclusive:GSHIFT".to_owned(), converted_device_action(5)),
                "DISABLE" => (
                    "exclusive:DISABLE".to_owned(),
                    ConvertedAction {
                        source_action: "disabled".into(),
                        daemon_action: None,
                        onboard_binding: "disabled".into(),
                        onboard_macro: None,
                        warnings: Vec::new(),
                    },
                ),
                "NONE" | "" => {
                    let modifiers = input_assignment
                        .get("modifierAssignments")
                        .and_then(Value::as_object);
                    if modifiers.is_some_and(|modifiers| modifiers.keys().any(|key| key != "0")) {
                        push_warning(
                            warnings,
                            "modifier_assignment_unsupported",
                            Some(&profile.name),
                            Some(settings_slot_id),
                            "modifier-gated assignments are not G-Shift and cannot be represented by the daemon",
                        );
                    }
                    let macro_id = modifiers
                        .and_then(|modifiers| modifiers.get("0"))
                        .and_then(|value| value.get("eventAssignments"))
                        .and_then(|value| value.get("1"))
                        .and_then(|value| value.get("macroId"))
                        .and_then(Value::as_str)
                        .unwrap_or("00000000-0000-0000-0000-000000000000");
                    let target_card = cards.get(macro_id).copied();
                    (macro_id.to_owned(), convert_card(macro_id, target_card)?)
                }
                value => {
                    push_warning(
                        warnings,
                        "exclusive_assignment_unsupported",
                        Some(&profile.name),
                        Some(settings_slot_id),
                        format!("unknown ExclusiveAssignment {value}"),
                    );
                    continue;
                }
            };
            let slot_id = format!(
                "{slot_prefix}_g{input_id}_m{mode}{}",
                if shifted { "_shifted" } else { "" }
            );
            for reason in &conversion.warnings {
                push_warning(
                    warnings,
                    warning_class(reason),
                    Some(&profile.name),
                    Some(&slot_id),
                    reason,
                );
            }
            if let Some(target_card) = cards.get(&card_id).copied()
                && target_card.get("attribute").and_then(Value::as_str) == Some("MACRO_PLAYBACK")
            {
                let converted = convert_macro(target_card.get("macro").unwrap_or(&Value::Null))?;
                referenced_macros
                    .entry(card_id.clone())
                    .or_insert(ImportedMacro {
                        card_id: card_id.clone(),
                        name: string(target_card, "name").unwrap_or_default(),
                        macro_type: converted.macro_type,
                        daemon_action: converted.daemon_action,
                        onboard_macro: converted.onboard_macro,
                        warnings: converted.warnings,
                    });
            }
            profile.bindings.push(ProfileBinding {
                slot_id,
                device_model: model.into(),
                slot_prefix: slot_prefix.into(),
                input: format!("g{input_id}"),
                mode,
                shifted,
                attribute: Some("MACRO_PLAYBACK".into()),
                card_id,
                source_action: conversion.source_action,
                daemon_action: conversion.daemon_action,
                onboard_binding: conversion.onboard_binding,
                onboard_macro: conversion.onboard_macro,
                warnings: conversion.warnings,
            });
        }
    }
    Ok(())
}

fn exclusive_assignment(value: &Value) -> String {
    let Some(value) = value.get("exclusiveAssignment") else {
        return "NONE".into();
    };
    if let Some(name) = value.as_str() {
        return name.to_ascii_uppercase();
    }
    match value.as_u64() {
        Some(0) | None => "NONE",
        Some(1) => "DISABLE",
        Some(2) => "GSHIFT",
        Some(_) => "UNKNOWN",
    }
    .into()
}

fn convert_card(card_id: &str, card: Option<&Value>) -> Result<ConvertedAction> {
    if let Some(builtin) = decode_builtin_card_id(card_id)? {
        return Ok(convert_builtin(builtin));
    }
    if matches!(
        card_id,
        "00000000-0000-0000-0000-000000000000" | "ffffffff-ffff-ffff-ffff-ffffffffffff"
    ) {
        return Ok(ConvertedAction {
            source_action: "unassigned".into(),
            daemon_action: None,
            onboard_binding: "disabled".into(),
            onboard_macro: None,
            warnings: vec!["assignment is empty; factory defaults are not stored in settings.db, so it remains unassigned".into()],
        });
    }
    let Some(card) = card else {
        return Ok(ConvertedAction {
            source_action: "missing_card".into(),
            daemon_action: None,
            onboard_binding: "disabled".into(),
            onboard_macro: None,
            warnings: vec![format!(
                "card {card_id} is absent and is not a recognized synthesized built-in; left unassigned"
            )],
        });
    };
    if card.get("attribute").and_then(Value::as_str) != Some("MACRO_PLAYBACK") {
        return Ok(ConvertedAction {
            source_action: format!(
                "card:{}",
                card.get("attribute")
                    .and_then(Value::as_str)
                    .unwrap_or("INVALID")
            ),
            daemon_action: None,
            onboard_binding: "noop".into(),
            onboard_macro: None,
            warnings: vec![
                "card is not a MACRO_PLAYBACK action; onboard binding replaced with NOOP".into(),
            ],
        });
    }
    let macro_value = card.get("macro").unwrap_or(&Value::Null);
    let converted = convert_macro(macro_value)?;
    let direct_binding = direct_onboard_binding(macro_value);
    let mut warnings = converted.warnings;
    if converted.onboard_macro.is_none()
        && direct_binding.is_none()
        && !warnings
            .iter()
            .any(|warning| warning.contains("onboard") || warning.contains("NOOP"))
    {
        warnings.push(format!(
            "Macro.Type {} cannot be stored in layout-A onboard memory; replaced with NOOP",
            converted.macro_type
        ));
    }
    Ok(ConvertedAction {
        source_action: format!("macro:{}", converted.macro_type),
        daemon_action: converted.daemon_action,
        onboard_binding: if converted.onboard_macro.is_some() {
            "macro:0:0".into()
        } else {
            direct_binding.unwrap_or_else(|| "noop".into())
        },
        onboard_macro: converted.onboard_macro,
        warnings,
    })
}

fn convert_builtin(builtin: BuiltinCard) -> ConvertedAction {
    match builtin {
        BuiltinCard::Keystroke { usage, modifiers } => match key_chord(usage, modifiers) {
            Some(keys) => ConvertedAction {
                source_action: format!("keystroke:{usage:02X}:{modifiers:02X}"),
                daemon_action: daemon_keys(keys.clone()),
                onboard_binding: format!("key:{keys}"),
                onboard_macro: None,
                warnings: Vec::new(),
            },
            None => noop_conversion(
                format!("keystroke:{usage:02X}:{modifiers:02X}"),
                format!("HID usage 0x{usage:02X} cannot be named for daemon/onboard import"),
            ),
        },
        BuiltinCard::MouseButton { button } => match onboard_mouse_button(button) {
            Some(binding) => ConvertedAction {
                source_action: format!("mouse:BUTTON:{button}"),
                daemon_action: None,
                onboard_binding: binding,
                onboard_macro: None,
                warnings: vec![
                    "mouse-button action is onboardable but is not a daemon action".into(),
                ],
            },
            None => noop_conversion(
                format!("mouse:BUTTON:{button}"),
                format!(
                    "mouse button {button} is outside the onboard range 1..5; replaced with NOOP"
                ),
            ),
        },
        BuiltinCard::MouseAction { action } => {
            let name = mouse_action_name(action);
            match onboard_mouse_action(action) {
                Some(binding) => ConvertedAction {
                    source_action: format!("mouse:{name}"),
                    daemon_action: None,
                    onboard_binding: binding.into(),
                    onboard_macro: None,
                    warnings: vec![format!(
                        "mouse action {name} is onboardable but is not a daemon action"
                    )],
                },
                None => noop_conversion(
                    format!("mouse:{name}"),
                    format!("mouse action {name} has no layout-A binding; replaced with NOOP"),
                ),
            }
        }
        BuiltinCard::DeviceAction { action } => converted_device_action(action),
        BuiltinCard::Unknown { kind, arg1, arg2 } => noop_conversion(
            format!("builtin:{kind:02X}:{arg1:02X}:{arg2:02X}"),
            format!("built-in card kind 0x{kind:02X} is unresolved; replaced with NOOP"),
        ),
    }
}

fn converted_device_action(action: u8) -> ConvertedAction {
    let name = device_action_name(action);
    let binding = match action {
        1 => Some("profile-cycle"),
        2 => Some("profile-next"),
        3 => Some("profile-prev"),
        5 => Some("g-shift"),
        6 => Some("battery-indicator"),
        7 => Some("inherit"),
        _ => None,
    };
    match binding {
        Some(binding) => ConvertedAction {
            source_action: format!("device:{name}"),
            daemon_action: None,
            onboard_binding: binding.into(),
            onboard_macro: None,
            warnings: if action == 7 {
                Vec::new()
            } else {
                vec![format!(
                    "device action {name} is onboardable but is not a daemon action"
                )]
            },
        },
        None => noop_conversion(
            format!("device:{name}"),
            format!("device action {name} has no layout-A binding; replaced with NOOP"),
        ),
    }
}

fn convert_macro(macro_value: &Value) -> Result<MacroConversion> {
    let macro_type = macro_value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("INVALID")
        .to_owned();
    let mut warnings = Vec::new();
    let (daemon_action, onboard_macro) = match macro_type.as_str() {
        "KEYSTROKE" => {
            let keys = macro_value.get("keystroke").and_then(keystroke_chord);
            match keys {
                Some(keys) => (daemon_keys(keys), None),
                None => {
                    warnings.push("KEYSTROKE contains an unsupported HID usage".into());
                    (None, None)
                }
            }
        }
        "TEXT_BLOCK" => {
            let text = macro_value
                .get("textBlock")
                .and_then(|value| value.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            warnings.push(
                "TEXT_BLOCK cannot be stored in layout-A onboard bytecode; replaced with NOOP"
                    .into(),
            );
            (Some(Action::Text(TextAction { text: text.into() })), None)
        }
        "MOUSE" => {
            warnings.push("MOUSE actions are not daemon macro steps".into());
            (None, None)
        }
        "SYSTEM" => {
            let action = macro_value
                .get("system")
                .and_then(|value| value.get("action"))
                .and_then(Value::as_str)
                .unwrap_or("INVALID");
            match system_keys(action) {
                Some(keys) => (daemon_keys(keys.into()), None),
                None => {
                    warnings.push(format!("SYSTEM action {action} is not mapped"));
                    (None, None)
                }
            }
        }
        "APP" | "OPEN_FILE_FOLDER" => match macro_value.get("app") {
            Some(app) => match app_action(app) {
                Some(action) => (Some(action), None),
                None => {
                    warnings.push(format!("{macro_type} has no executablePath"));
                    (None, None)
                }
            },
            None => {
                warnings.push(format!("{macro_type} has no app payload"));
                (None, None)
            }
        },
        "AUDIO" => {
            let action = macro_value
                .get("audio")
                .and_then(|value| value.get("action"))
                .and_then(Value::as_str)
                .unwrap_or("INVALID");
            match audio_key(action) {
                Some(keys) => (daemon_keys(keys.into()), None),
                None => {
                    warnings.push(format!("AUDIO action {action} is not mapped"));
                    (None, None)
                }
            }
        }
        "MEDIA" => {
            let usage = macro_value
                .get("media")
                .and_then(|value| value.get("usage"))
                .and_then(Value::as_str)
                .unwrap_or("INVALID");
            match media_key(usage) {
                Some(keys) => (daemon_keys(keys.into()), None),
                None => {
                    warnings.push(format!("MEDIA usage {usage} is not mapped"));
                    (None, None)
                }
            }
        }
        "SEQUENCE" => {
            let sequence = macro_value
                .get("sequence")
                .or_else(|| macro_value.get("sequences"));
            match sequence {
                Some(sequence) => convert_sequence(
                    sequence,
                    macro_value
                        .get("onboardable")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    &mut warnings,
                )?,
                None => {
                    warnings.push("SEQUENCE has neither sequence nor sequences".into());
                    (None, None)
                }
            }
        }
        "DEVICE" => {
            let action = macro_value
                .get("device")
                .and_then(|value| value.get("action"))
                .and_then(Value::as_str)
                .unwrap_or("INVALID");
            warnings.push(format!("DEVICE action {action} is not a daemon action"));
            (None, None)
        }
        "OPEN_WEB_PAGE" => {
            let url = macro_value
                .get("openWebPage")
                .and_then(|value| value.get("url"))
                .and_then(Value::as_str)
                .or_else(|| {
                    macro_value
                        .get("app")
                        .and_then(|value| value.get("executablePath"))
                        .and_then(Value::as_str)
                });
            match url {
                Some(url) => (
                    Some(Action::Run(RunAction {
                        run: format!("start \"\" {}", quote_cmd(url)),
                    })),
                    None,
                ),
                None => {
                    warnings.push("OPEN_WEB_PAGE has no URL".into());
                    (None, None)
                }
            }
        }
        "SCREEN_CAPTURE" => (daemon_keys("win+shift+s".into()), None),
        "QUICK_LAUNCH" => (
            Some(Action::Run(RunAction {
                run: "calc.exe".into(),
            })),
            None,
        ),
        "LIGHTING"
        | "GHUB"
        | "ACTION"
        | "AUDIO_SETTINGS"
        | "AUDIO_SAMPLE"
        | "ILLUMINATION_LIGHT_PRESET"
        | "ILLUMINATION_LIGHT"
        | "LPS_ACTION"
        | "PROFILES"
        | "INVALID" => {
            warnings.push(format!(
                "Macro.Type {macro_type} has no daemon/onboard mapping"
            ));
            (None, None)
        }
        other => {
            warnings.push(format!("unknown Macro.Type {other}"));
            (None, None)
        }
    };
    Ok(MacroConversion {
        macro_type,
        daemon_action,
        onboard_macro,
        warnings,
    })
}

fn convert_sequence(
    sequence: &Value,
    onboardable: bool,
    warnings: &mut Vec<String>,
) -> Result<(Option<Action>, Option<OnboardMacro>)> {
    let (bucket, behavior_warning) = if sequence
        .get("useRepeatActions")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        (
            "heldSequence",
            Some("repeat-while-held macro is imported as one daemon pass"),
        )
    } else if sequence
        .get("useSimpleActions")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        ("simpleSequence", None)
    } else if sequence
        .get("useToggleActions")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        (
            "toggleSequence",
            Some("toggle macro is imported as one daemon pass"),
        )
    } else {
        (
            "pressSequence",
            Some("press/held/release macro is imported from pressSequence only"),
        )
    };
    if let Some(reason) = behavior_warning {
        warnings.push(reason.into());
    }
    let components = sequence
        .get(bucket)
        .and_then(|value| value.get("components"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let daemon = daemon_sequence(components, warnings);
    let onboard = if onboardable {
        onboard_sequence(components, warnings)
    } else {
        warnings
            .push("SEQUENCE is not marked onboardable; onboard binding replaced with NOOP".into());
        None
    };
    Ok((
        daemon.map(|steps| Action::Macro(MacroAction { r#macro: steps })),
        onboard.map(|steps| OnboardMacro { steps }),
    ))
}

fn daemon_sequence(
    components: &[Value],
    warnings: &mut Vec<String>,
) -> Option<Vec<DaemonMacroStep>> {
    let mut held = BTreeSet::<String>::new();
    let mut steps = Vec::new();
    let mut unsupported = false;
    for component in components {
        if let Some(keyboard) = component.get("keyboard") {
            let usage = u64_field(keyboard, "hidUsage").and_then(|value| u8::try_from(value).ok());
            let Some(usage) = usage else {
                unsupported = true;
                continue;
            };
            let is_down = keyboard
                .get("isDown")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(modifier) = modifier_for_usage(usage, false) {
                if is_down {
                    held.insert(modifier.into());
                } else {
                    held.remove(modifier);
                }
            } else if is_down {
                let Some(key) = daemon_key_name(usage) else {
                    unsupported = true;
                    continue;
                };
                let mut chord = held.iter().cloned().collect::<Vec<_>>();
                chord.push(key.into());
                steps.push(DaemonMacroStep::Keys(KeysAction {
                    keys: chord.join("+"),
                }));
            }
        } else if let Some(delay) = component.get("delay") {
            let duration = delay
                .get("durationMs")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            steps.push(DaemonMacroStep::Delay(DelayStep { delay_ms: duration }));
        } else if let Some(text) = component
            .get("textBlock")
            .and_then(|value| value.get("text"))
            .and_then(Value::as_str)
        {
            steps.push(DaemonMacroStep::Text(TextAction { text: text.into() }));
        } else if let Some(keys) = component.get("keystroke").and_then(keystroke_chord) {
            steps.push(DaemonMacroStep::Keys(KeysAction { keys }));
        } else if let Some(usage) = component
            .get("media")
            .and_then(|value| value.get("usage"))
            .and_then(Value::as_str)
        {
            if let Some(keys) = media_key(usage) {
                steps.push(DaemonMacroStep::Keys(KeysAction { keys: keys.into() }));
            } else {
                unsupported = true;
            }
        } else if let Some(action) = component
            .get("system")
            .and_then(|value| value.get("action"))
            .and_then(Value::as_str)
        {
            if let Some(keys) = system_keys(action) {
                steps.push(DaemonMacroStep::Keys(KeysAction { keys: keys.into() }));
            } else {
                unsupported = true;
            }
        } else {
            unsupported = true;
        }
    }
    if unsupported {
        warnings.push("sequence contains mouse/app/plugin/device or unknown steps that the daemon cannot preserve".into());
        return None;
    }
    if steps.is_empty() {
        warnings.push("sequence contains no daemon steps".into());
        None
    } else {
        Some(steps)
    }
}

fn onboard_sequence(
    components: &[Value],
    warnings: &mut Vec<String>,
) -> Option<Vec<OnboardMacroStep>> {
    let mut steps = Vec::new();
    for component in components {
        if let Some(keyboard) = component.get("keyboard") {
            let usage =
                u64_field(keyboard, "hidUsage").and_then(|value| u8::try_from(value).ok())?;
            let key = onboard_key_name(usage)?;
            if keyboard
                .get("isDown")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                steps.push(OnboardMacroStep::KeyPress { key_press: key });
            } else {
                steps.push(OnboardMacroStep::KeyRelease { key_release: key });
            }
        } else if let Some(delay) = component.get("delay") {
            let value = delay
                .get("durationMs")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok());
            let Some(delay_ms) = value else {
                warnings.push("onboard delay exceeds 65535 ms".into());
                return None;
            };
            steps.push(OnboardMacroStep::Delay { delay_ms });
        } else if let Some(keys) = component.get("keystroke").and_then(keystroke_chord) {
            steps.push(OnboardMacroStep::Key { key: keys });
        } else if let Some(usage) = component
            .get("media")
            .and_then(|value| value.get("usage"))
            .and_then(Value::as_str)
        {
            let Some(consumer) = media_key(usage) else {
                warnings.push(format!("MEDIA usage {usage} is not onboardable"));
                return None;
            };
            steps.push(OnboardMacroStep::Consumer {
                consumer: consumer.into(),
            });
        } else {
            warnings.push(
                "onboard macros accept only keyboard, delay, keystroke, and consumer steps".into(),
            );
            return None;
        }
    }
    (!steps.is_empty()).then_some(steps)
}

fn direct_onboard_binding(macro_value: &Value) -> Option<String> {
    match macro_value.get("type")?.as_str()? {
        "KEYSTROKE" => macro_value
            .get("keystroke")
            .and_then(keystroke_chord)
            .map(|keys| format!("key:{keys}")),
        "MEDIA" => macro_value
            .get("media")
            .and_then(|value| value.get("usage"))
            .and_then(Value::as_str)
            .and_then(media_key)
            .map(|key| format!("consumer:{key}")),
        "SYSTEM" => macro_value
            .get("system")
            .and_then(|value| value.get("action"))
            .and_then(Value::as_str)
            .and_then(system_keys)
            .map(|keys| format!("key:{keys}")),
        "AUDIO" => macro_value
            .get("audio")
            .and_then(|value| value.get("action"))
            .and_then(Value::as_str)
            .and_then(audio_key)
            .map(|key| format!("consumer:{key}")),
        "SCREEN_CAPTURE" => Some("key:win+shift+s".into()),
        "MOUSE" => {
            let mouse = macro_value.get("mouse")?;
            let action = mouse.get("action")?.as_str()?;
            if action == "BUTTON" {
                u64_field(mouse, "hidUsage")
                    .and_then(|button| u8::try_from(button).ok())
                    .and_then(onboard_mouse_button)
            } else {
                onboard_mouse_action_name(action).map(str::to_owned)
            }
        }
        "DEVICE" => macro_value
            .get("device")
            .and_then(|value| value.get("action"))
            .and_then(Value::as_str)
            .and_then(onboard_device_action_name)
            .map(str::to_owned),
        _ => None,
    }
}

fn onboard_mouse_button(button: u8) -> Option<String> {
    Some(
        match button {
            1 => "mouse:left",
            2 => "mouse:right",
            3 => "mouse:middle",
            4 => "mouse:back",
            5 => "mouse:forward",
            _ => return None,
        }
        .into(),
    )
}

fn lighting_presets(card: &Value) -> Result<Vec<RgbPreset>> {
    let attribute = card
        .get("attribute")
        .and_then(Value::as_str)
        .unwrap_or("INVALID");
    let (settings, effects_field) = match attribute {
        "LIGHTING_SETTINGS" => (
            card.get("lightingSettings")
                .context("LIGHTING_SETTINGS card has no lightingSettings")?,
            "firmwareEffects",
        ),
        "FIRMWARE_LIGHTING_SETTINGS" => (
            card.get("firmwareLightingSettings")
                .context("firmware card has no firmwareLightingSettings")?,
            "effects",
        ),
        "SYNC_LIGHTING_SETTINGS" => (
            card.get("syncLightingSettings")
                .context("sync card has no syncLightingSettings")?,
            "effects",
        ),
        "SOFTWARE_LIGHTING_SETTINGS" => {
            bail!("software lighting has no firmware/onboard RGB invocation")
        }
        _ => bail!("card attribute {attribute} is not a lighting settings card"),
    };
    let effects = settings
        .get(effects_field)
        .and_then(Value::as_array)
        .context("lighting card has no firmware effects")?;
    let brightness = settings.get("brightness").and_then(|value| {
        if value.get("isOff").and_then(Value::as_bool) == Some(true) {
            Some(0)
        } else {
            value
                .get("value")
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
        }
    });
    let mut presets = effects
        .iter()
        .map(|effect| lighting_preset(effect, brightness))
        .collect::<Result<Vec<_>>>()?;
    if attribute == "FIRMWARE_LIGHTING_SETTINGS" {
        for effect in settings
            .get("powerSavingEffects")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let mut preset = lighting_preset(effect, brightness)?;
            preset.persist = "powersave".into();
            presets.push(preset);
        }
    }
    Ok(presets)
}

fn lighting_preset(effect: &Value, brightness: Option<u8>) -> Result<RgbPreset> {
    let id = effect
        .get("id")
        .and_then(Value::as_str)
        .context("firmware effect has no id")?;
    let params = [
        "fixedParams",
        "breathingParams",
        "cycleParams",
        "colorwaveParams",
        "rippleParams",
        "customParams",
        "kittParams",
        "decompositionParams",
        "frameDataParams",
        "dualColorParams",
    ]
    .iter()
    .find_map(|key| effect.get(*key))
    .unwrap_or(&Value::Null);
    let effect_name = match id {
        "COLORWAVE" => "colorwave",
        "COLOR_CYCLE_S" => "color_cycle_s",
        "COLORWAVE_S" => "colorwave_s",
        "RIPPLE_S" => "ripple_s",
        "SNIPE_PULSE_CP" => "signature_frame_active",
        "NEURAL_WAVE_CP" => "signature_frame_inactive",
        "SMOOTH_STAR" => "signature_algorithmic_active",
        "SMOOTH_WAVE" => "signature_algorithmic_inactive",
        value => value,
    }
    .to_ascii_lowercase();
    let color = params
        .get("color")
        .or_else(|| params.get("colorOne"))
        .or_else(|| params.get("signatureColorOne"))
        .and_then(color_hex);
    let color2 = params
        .get("colorTwo")
        .or_else(|| params.get("bgColor"))
        .or_else(|| params.get("signatureColorTwo"))
        .and_then(color_hex);
    let intensity = params.get("intensity").and_then(percent_float);
    let period = params
        .get("periodInMs")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok());
    let speed = params
        .get("frameRate")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok());
    Ok(RgbPreset {
        zone: effect
            .get("zoneType")
            .and_then(Value::as_str)
            .unwrap_or("ZONE_ALL")
            .into(),
        effect: effect_name,
        color,
        color2,
        speed,
        period,
        brightness,
        intensity,
        direction: params
            .get("direction")
            .and_then(Value::as_str)
            .map(|value| value.to_ascii_lowercase()),
        persist: if effect
            .get("persistent")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "nvm"
        } else {
            "ram"
        }
        .into(),
    })
}

fn build_daemon_bindings(
    profiles: &[Profile],
    warnings: &mut Vec<ImportWarning>,
) -> Result<Bindings> {
    let Some(selected) = profiles
        .iter()
        .find(|profile| profile.name.eq_ignore_ascii_case("Desktop"))
        .or_else(|| profiles.first())
    else {
        return Ok(Bindings::default());
    };
    if profiles.len() > 1 {
        push_warning(
            warnings,
            "daemon_profile_selection",
            Some(&selected.name),
            None,
            "bindings.json contains one daemon map; Desktop was selected and other application profiles remain in profiles.json",
        );
    }
    let inferred_cids = infer_mouse_cids(profiles);
    let mut output = Bindings::default();
    for binding in &selected.bindings {
        let Some(action) = &binding.daemon_action else {
            continue;
        };
        if binding.shifted {
            push_warning(
                warnings,
                "daemon_gshift_unsupported",
                Some(&selected.name),
                Some(&binding.slot_id),
                "bindings.json has no G-Shift layer; action remains in profiles.json and the onboard artifact",
            );
            continue;
        }
        if binding.mode != 1 {
            push_warning(
                warnings,
                "daemon_mode_unsupported",
                Some(&selected.name),
                Some(&binding.slot_id),
                format!(
                    "bindings.json has no M{} mode map; action remains in profiles.json and the onboard artifact",
                    binding.mode
                ),
            );
            continue;
        }
        let device = output
            .devices
            .entry(binding.device_model.clone())
            .or_insert_with(DeviceBindings::default);
        let input_number = binding
            .input
            .strip_prefix('g')
            .and_then(|value| value.parse::<u16>().ok());
        let model = device_data::lookup_model(&binding.device_model);
        if let Some(number) = input_number
            && model
                .and_then(|model| model.gkeys.count)
                .is_some_and(|count| (1..=count).contains(&number))
        {
            device.gkeys.insert(binding.input.clone(), action.clone());
        } else if let Some(cid) =
            inferred_cids.get(&(binding.device_model.clone(), binding.input.clone()))
        {
            device.cids.insert(format!("0x{cid:04X}"), action.clone());
        } else {
            push_warning(
                warnings,
                "daemon_input_unresolved",
                Some(&selected.name),
                Some(&binding.slot_id),
                "settings.db has no physical slot-to-CID map; action remains available for onboard import only",
            );
        }
    }
    output
        .devices
        .retain(|_, device| !device.gkeys.is_empty() || !device.cids.is_empty());
    Ok(output)
}

fn infer_mouse_cids(profiles: &[Profile]) -> BTreeMap<(String, String), u16> {
    let mut candidates = BTreeMap::<(String, String), BTreeSet<u16>>::new();
    for binding in profiles.iter().flat_map(|profile| &profile.bindings) {
        if let Some(cid) = cid_for_source(&binding.source_action) {
            candidates
                .entry((binding.device_model.clone(), binding.input.clone()))
                .or_default()
                .insert(cid);
        }
    }
    candidates
        .into_iter()
        .filter_map(|(key, values)| (values.len() == 1).then(|| (key, *values.first().unwrap())))
        .collect()
}

fn cid_for_source(source: &str) -> Option<u16> {
    Some(match source {
        "mouse:BUTTON:1" => 0x0050,
        "mouse:BUTTON:2" => 0x0051,
        "mouse:BUTTON:3" => 0x0052,
        "mouse:BUTTON:4" => 0x0053,
        "mouse:BUTTON:5" => 0x0056,
        "mouse:DPI_UP" => 0x005B,
        "mouse:DPI_DOWN" => 0x005D,
        "mouse:DPI_SHIFT" | "mouse:DPI_GO_TO_SHIFT" => 0x00E0,
        _ => return None,
    })
}

fn add_unassigned_slots(profiles: &mut [Profile], warnings: &mut Vec<ImportWarning>) {
    let universe = profiles
        .iter()
        .flat_map(|profile| {
            profile.bindings.iter().map(|binding| {
                (
                    binding.device_model.clone(),
                    binding.slot_prefix.clone(),
                    binding.input.clone(),
                    binding.mode,
                    binding.shifted,
                    binding.attribute.clone(),
                )
            })
        })
        .collect::<BTreeSet<_>>();
    for profile in profiles {
        let models = profile
            .device_models
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let existing = profile
            .bindings
            .iter()
            .map(|binding| {
                (
                    binding.device_model.clone(),
                    binding.input.clone(),
                    binding.mode,
                    binding.shifted,
                )
            })
            .collect::<BTreeSet<_>>();
        for (model, prefix, input, mode, shifted, attribute) in &universe {
            if !models.contains(model)
                || existing.contains(&(model.clone(), input.clone(), *mode, *shifted))
            {
                continue;
            }
            let slot_id = format!(
                "{prefix}_{input}_m{mode}{}{}",
                if *shifted { "_shifted" } else { "" },
                attribute
                    .as_ref()
                    .map(|attribute| format!(":{attribute}"))
                    .unwrap_or_default()
            );
            let reason = "assignment is absent; device-depot defaults are not in settings.db, so it was imported as unassigned";
            profile.bindings.push(ProfileBinding {
                slot_id: slot_id.clone(),
                device_model: model.clone(),
                slot_prefix: prefix.clone(),
                input: input.clone(),
                mode: *mode,
                shifted: *shifted,
                attribute: attribute.clone(),
                card_id: String::new(),
                source_action: "unassigned".into(),
                daemon_action: None,
                onboard_binding: "disabled".into(),
                onboard_macro: None,
                warnings: vec![reason.into()],
            });
            push_warning(
                warnings,
                "default_assignment_missing",
                Some(&profile.name),
                Some(&slot_id),
                reason,
            );
        }
        profile
            .bindings
            .sort_by(|left, right| left.slot_id.cmp(&right.slot_id));
    }
}

fn resolve_requested_model(value: &str) -> Result<String> {
    device_data::lookup_model(value)
        .map(|model| model.model_id.clone())
        .with_context(|| format!("unknown device model {value:?}"))
}

fn resolve_slot_model(prefix: &str) -> String {
    device_data::lookup_slot_prefix(prefix)
        .map(|model| model.model_id.clone())
        .unwrap_or_else(|| prefix.replace('-', "_"))
}

fn model_selected(model: &str, requested: Option<&str>) -> bool {
    requested.is_none_or(|requested| model.eq_ignore_ascii_case(requested))
}

fn insert_model(models: &mut Vec<String>, model: String) {
    if !models.contains(&model) {
        models.push(model);
    }
}

fn parse_layer(layer: &str) -> Result<(u8, bool)> {
    match layer {
        "0" | "LAYER_BASE" => Ok((1, false)),
        "2" | "LAYER_G" => Ok((1, true)),
        "10" => Ok((1, false)),
        "11" => Ok((1, true)),
        "12" => Ok((2, false)),
        "13" => Ok((2, true)),
        "14" => Ok((3, false)),
        "15" => Ok((3, true)),
        "1" | "LAYER_FN" => bail!("FN layer is not an M-mode/G-Shift layer"),
        _ => {
            let shifted = layer.ends_with("_G");
            let base = layer.strip_suffix("_G").unwrap_or(layer);
            let mode = base
                .strip_prefix("LAYER_MODE")
                .context("unsupported input layer")?
                .parse::<u8>()
                .context("invalid input layer mode")?;
            Ok((mode, shifted))
        }
    }
}

fn find_object<'a>(value: &'a Value, key: &str) -> Option<&'a serde_json::Map<String, Value>> {
    if let Some(found) = value.get(key).and_then(Value::as_object) {
        return Some(found);
    }
    value
        .as_object()?
        .values()
        .find_map(|child| find_object(child, key))
}

fn slot_id_contains_mode(slot_id: &str) -> bool {
    slot_id.rfind("_m").is_some_and(|position| {
        slot_id[position + 2..]
            .split(['_', ':'])
            .next()
            .is_some_and(|value| {
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
            })
    })
}

fn display_application_name(name: &str) -> String {
    if name == "APPLICATION_NAME_DESKTOP" {
        "Desktop".into()
    } else {
        name.to_owned()
    }
}

fn is_lighting_attribute(attribute: &str) -> bool {
    matches!(
        attribute,
        "FIRMWARE_LIGHTING_SETTINGS"
            | "SOFTWARE_LIGHTING_SETTINGS"
            | "SYNC_LIGHTING_SETTINGS"
            | "LIGHTING_SETTINGS"
    )
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn u16_field(value: &Value, key: &str) -> Result<u16> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .with_context(|| format!("{key} is missing or outside u16"))
}

fn u64_field(value: &Value, key: &str) -> Option<u64> {
    value
        .get(key)
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

fn daemon_keys(keys: String) -> Option<Action> {
    Some(Action::Keys(KeysAction { keys }))
}

fn keystroke_chord(value: &Value) -> Option<String> {
    let usage = value
        .get("code")
        .and_then(Value::as_u64)
        .and_then(|value| u8::try_from(value).ok())
        .unwrap_or(0);
    let mut mask = 0_u8;
    for modifier in value
        .get("modifiers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let usage = modifier
            .as_u64()
            .and_then(|value| u8::try_from(value).ok())?;
        mask |= 1_u8.checked_shl(u32::from(usage.checked_sub(0xE0)?))?;
    }
    key_chord(usage, mask)
}

fn key_chord(usage: u8, modifiers: u8) -> Option<String> {
    let mut keys = Vec::new();
    for (mask, name) in [
        (0x01, "ctrl"),
        (0x02, "shift"),
        (0x04, "alt"),
        (0x08, "win"),
        (0x10, "ctrl"),
        (0x20, "shift"),
        (0x40, "alt"),
        (0x80, "win"),
    ] {
        if modifiers & mask != 0 && !keys.contains(&name) {
            keys.push(name);
        }
    }
    if usage != 0 {
        let key = modifier_for_usage(usage, false).or_else(|| daemon_key_name(usage))?;
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    (!keys.is_empty()).then(|| keys.join("+"))
}

fn daemon_key_name(usage: u8) -> Option<&'static str> {
    const LETTERS: [&str; 26] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t", "u", "v", "w", "x", "y", "z",
    ];
    const DIGITS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];
    const FUNCTIONS: [&str; 24] = [
        "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14",
        "f15", "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24",
    ];
    match usage {
        0x04..=0x1D => Some(LETTERS[usize::from(usage - 0x04)]),
        0x1E..=0x27 => Some(DIGITS[usize::from(usage - 0x1E)]),
        0x3A..=0x45 => Some(FUNCTIONS[usize::from(usage - 0x3A)]),
        0x68..=0x73 => Some(FUNCTIONS[usize::from(usage - 0x68 + 12)]),
        0x28 => Some("enter"),
        0x29 => Some("esc"),
        0x2A => Some("backspace"),
        0x2B => Some("tab"),
        0x2C => Some("space"),
        0x2D => Some("minus"),
        0x2E => Some("equal"),
        0x2F => Some("leftbracket"),
        0x30 => Some("rightbracket"),
        0x31 => Some("backslash"),
        0x33 => Some("semicolon"),
        0x34 => Some("quote"),
        0x35 => Some("grave"),
        0x36 => Some("comma"),
        0x37 => Some("period"),
        0x38 => Some("slash"),
        0x39 => Some("capslock"),
        0x46 => Some("printscreen"),
        0x47 => Some("scrolllock"),
        0x48 => Some("pause"),
        0x49 => Some("insert"),
        0x4A => Some("home"),
        0x4B => Some("pageup"),
        0x4C => Some("delete"),
        0x4D => Some("end"),
        0x4E => Some("pagedown"),
        0x4F => Some("right"),
        0x50 => Some("left"),
        0x51 => Some("down"),
        0x52 => Some("up"),
        _ => None,
    }
}

fn onboard_key_name(usage: u8) -> Option<String> {
    modifier_for_usage(usage, true)
        .map(str::to_owned)
        .or_else(|| daemon_key_name(usage).map(str::to_owned))
        .or_else(|| Some(format!("usage0x{usage:02X}")))
}

fn modifier_for_usage(usage: u8, preserve_side: bool) -> Option<&'static str> {
    Some(match (usage, preserve_side) {
        (0xE0, _) => "ctrl",
        (0xE1, _) => "shift",
        (0xE2, _) => "alt",
        (0xE3, _) => "win",
        (0xE4, true) => "rctrl",
        (0xE5, true) => "rshift",
        (0xE6, true) => "ralt",
        (0xE7, true) => "rwin",
        (0xE4, false) => "ctrl",
        (0xE5, false) => "shift",
        (0xE6, false) => "alt",
        (0xE7, false) => "win",
        _ => return None,
    })
}

fn system_keys(action: &str) -> Option<&'static str> {
    Some(match action {
        "CLOSE" => "alt+f4",
        "COPY" => "ctrl+c",
        "PASTE" => "ctrl+v",
        "UNDO" => "ctrl+z",
        "REDO" => "ctrl+y",
        "DESKTOP" | "SHOW_DESKTOP" => "win+d",
        "SWITCH_APPS" => "alt+tab",
        "SHIFT_SWITCH_APPS" => "alt+shift+tab",
        "SNAP_LEFT" => "win+left",
        "SNAP_RIGHT" => "win+right",
        "SCREEN_CAPTURE" => "win+shift+s",
        "LOCK" => "win+l",
        _ => return None,
    })
}

fn media_key(usage: &str) -> Option<&'static str> {
    Some(match usage {
        "PLAY_PAUSE" => "media-play-pause",
        "STOP" => "media-stop",
        "VOLUME_UP" => "volume-up",
        "VOLUME_DOWN" => "volume-down",
        "NEXT_TRACK" => "media-next",
        "PREVIOUS_TRACK" => "media-prev",
        "MUTE" => "volume-mute",
        _ => return None,
    })
}

fn audio_key(action: &str) -> Option<&'static str> {
    Some(match action {
        "MUTE_SPEAKERS" => "volume-mute",
        _ => return None,
    })
}

fn app_action(app: &Value) -> Option<Action> {
    let executable = app.get("executablePath")?.as_str()?;
    let mut command = quote_cmd(executable);
    for argument in app
        .get("argumentList")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        command.push(' ');
        command.push_str(&quote_cmd(argument.as_str()?));
    }
    Some(Action::Run(RunAction { run: command }))
}

fn quote_cmd(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn mouse_action_name(action: u8) -> &'static str {
    match action {
        1 => "DPI_UP",
        2 => "DPI_DOWN",
        3 => "DPI_SHIFT",
        4 => "DPI_CYCLE",
        5 => "DPI_DEFAULT",
        6 => "BUTTON",
        8 => "BUTTON_DOUBLE_CLICK",
        9 => "WHEEL_UP",
        10 => "WHEEL_DOWN",
        11 => "WHEEL_LEFT",
        12 => "WHEEL_RIGHT",
        14 => "DPI_GO_TO_SHIFT",
        15 => "WHEEL_MODE_TOGGLE",
        16 => "RATCHET_FORCE_CYCLE",
        _ => "INVALID",
    }
}

fn onboard_mouse_action(action: u8) -> Option<&'static str> {
    onboard_mouse_action_name(mouse_action_name(action))
}

fn onboard_mouse_action_name(action: &str) -> Option<&'static str> {
    Some(match action {
        "DPI_UP" => "dpi-up",
        "DPI_DOWN" => "dpi-down",
        "DPI_SHIFT" | "DPI_GO_TO_SHIFT" => "dpi-shift",
        "DPI_CYCLE" => "dpi-cycle",
        "DPI_DEFAULT" => "dpi-default",
        "WHEEL_UP" => "scroll-up",
        "WHEEL_DOWN" => "scroll-down",
        "WHEEL_LEFT" => "tilt-left",
        "WHEEL_RIGHT" => "tilt-right",
        "WHEEL_MODE_TOGGLE" => "wheel-mode-toggle",
        "RATCHET_FORCE_CYCLE" => "ratchet-force-cycle",
        _ => return None,
    })
}

fn device_action_name(action: u8) -> &'static str {
    match action {
        1 => "PROFILE_CYCLE",
        2 => "PROFILE_NEXT",
        3 => "PROFILE_PREV",
        4 => "PROFILE_ACTIVATE",
        5 => "G_SHIFT",
        6 => "BATTERY_LIFE",
        7 => "G_SHIFT_DEFAULT",
        8 => "NATIVE_ACTION",
        9 => "REPORT_RATE_CYCLE",
        _ => "INVALID",
    }
}

fn onboard_device_action_name(action: &str) -> Option<&'static str> {
    Some(match action {
        "PROFILE_CYCLE" => "profile-cycle",
        "PROFILE_NEXT" => "profile-next",
        "PROFILE_PREV" => "profile-prev",
        "G_SHIFT" => "g-shift",
        "BATTERY_LIFE" => "battery-indicator",
        "G_SHIFT_DEFAULT" => "inherit",
        _ => return None,
    })
}

fn noop_conversion(source_action: String, reason: String) -> ConvertedAction {
    ConvertedAction {
        source_action,
        daemon_action: None,
        onboard_binding: "noop".into(),
        onboard_macro: None,
        warnings: vec![reason],
    }
}

fn color_hex(value: &Value) -> Option<String> {
    if let Some(hex) = value.get("hex").and_then(Value::as_str) {
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        if hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Some(hex.to_ascii_uppercase());
        }
    }
    let component = |name| {
        value
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
    };
    Some(format!(
        "{:02X}{:02X}{:02X}",
        component("red").or_else(|| component("r"))?,
        component("green").or_else(|| component("g"))?,
        component("blue").or_else(|| component("b"))?
    ))
}

fn percent_float(value: &Value) -> Option<u8> {
    let value = value.as_f64()?;
    let percent = if value <= 1.0 { value * 100.0 } else { value };
    Some(percent.round().clamp(0.0, 100.0) as u8)
}

fn safe_name(value: &str) -> String {
    let mut result = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    while result.contains("--") {
        result = result.replace("--", "-");
    }
    result.trim_matches('-').to_ascii_lowercase()
}

fn warning_class(reason: &str) -> &'static str {
    if reason.contains("daemon") {
        "daemon_unsupported"
    } else if reason.contains("NOOP") || reason.contains("onboard") {
        "onboard_unsupported"
    } else if reason.contains("absent") || reason.contains("unassigned") {
        "default_assignment_missing"
    } else {
        "action_unsupported"
    }
}

fn push_warning(
    warnings: &mut Vec<ImportWarning>,
    class: impl Into<String>,
    profile: Option<&str>,
    slot_id: Option<&str>,
    reason: impl Into<String>,
) {
    warnings.push(ImportWarning {
        class: class.into(),
        profile: profile.map(str::to_owned),
        slot_id: slot_id.map(str::to_owned),
        reason: reason.into(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/ghub_minimal.json");

    #[test]
    fn decodes_all_known_builtin_card_kinds() {
        assert_eq!(
            decode_builtin_card_id("0f82f693-5b78-4cf5-867e-010608000000").unwrap(),
            Some(BuiltinCard::Keystroke {
                usage: 6,
                modifiers: 8
            })
        );
        assert_eq!(
            decode_builtin_card_id("0f82f693-5b78-4cf5-867e-020500000000").unwrap(),
            Some(BuiltinCard::MouseButton { button: 5 })
        );
        assert_eq!(
            decode_builtin_card_id("0f82f693-5b78-4cf5-867e-040b00000000").unwrap(),
            Some(BuiltinCard::MouseAction { action: 11 })
        );
        assert_eq!(
            decode_builtin_card_id("0f82f693-5b78-4cf5-867e-090700000000").unwrap(),
            Some(BuiltinCard::DeviceAction { action: 7 })
        );
        assert_eq!(
            decode_builtin_card_id("0f82f693-5b78-4cf5-867e-080200000000").unwrap(),
            Some(BuiltinCard::Unknown {
                kind: 8,
                arg1: 2,
                arg2: 0
            })
        );
        assert_eq!(
            convert_builtin(BuiltinCard::Keystroke {
                usage: 0xE0,
                modifiers: 0,
            })
            .onboard_binding,
            "key:ctrl"
        );
    }

    #[test]
    fn parses_slot_ids_from_the_right_with_prefix_underscores() {
        assert_eq!(
            parse_slot_id("proxtkl_rapid_g12_m3_shifted:MACRO_PLAYBACK").unwrap(),
            ParsedSlotId {
                slot_prefix: "proxtkl_rapid".into(),
                input: "g12".into(),
                mode: Some(3),
                shifted: true,
                attribute: Some("MACRO_PLAYBACK".into()),
            }
        );
        assert_eq!(
            parse_slot_id("g502x-lightspeed_mouse_settings")
                .unwrap()
                .slot_prefix,
            "g502x-lightspeed"
        );
    }

    #[test]
    fn converts_sequence_and_editor_sequences_alias() {
        let macro_value = serde_json::json!({
            "type":"SEQUENCE","onboardable":true,
            "sequences":{"useSimpleActions":true,"simpleSequence":{"components":[
                {"keyboard":{"hidUsage":"224","isDown":true}},
                {"keyboard":{"hidUsage":"6","isDown":true}},
                {"delay":{"durationMs":50}},
                {"keyboard":{"hidUsage":"6","isDown":false}},
                {"keyboard":{"hidUsage":"224","isDown":false}}
            ]}}
        });
        let converted = convert_macro(&macro_value).unwrap();
        assert!(matches!(converted.daemon_action, Some(Action::Macro(_))));
        assert_eq!(converted.onboard_macro.unwrap().steps.len(), 5);
        assert_eq!(
            direct_onboard_binding(&serde_json::json!({
                "type":"MOUSE","mouse":{"action":"BUTTON","hidUsage":"5"}
            })),
            Some("mouse:forward".into())
        );
    }

    #[test]
    fn converts_dpi_values_to_onboard_indices() {
        assert_eq!(
            dpi_indices(&[800, 1200, 1600, 4000, 7000], 4000, 800).unwrap(),
            (3, Some(0))
        );
        assert!(dpi_indices(&[800, 1600], 3200, 800).is_err());
    }

    #[test]
    fn converts_firmware_and_power_saving_lighting_cards() {
        let card = serde_json::json!({
            "attribute":"FIRMWARE_LIGHTING_SETTINGS",
            "firmwareLightingSettings":{
                "effects":[{"id":"FIXED","zoneType":"ZONE_PRIMARY","persistent":true,
                    "fixedParams":{"color":{"hex":"#010203"}}}],
                "powerSavingEffects":[{"id":"OFF","zoneType":"ZONE_PRIMARY"}],
                "brightness":{"value":75,"isOff":false}
            }
        });
        let presets = lighting_presets(&card).unwrap();
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].persist, "nvm");
        assert_eq!(presets[0].brightness, Some(75));
        assert_eq!(presets[1].persist, "powersave");
    }

    #[test]
    fn follows_proto_layer_presets_and_keeps_gshift_exclusive() {
        let document = serde_json::json!({
            "profiles":{"profiles":[{
                "id":"p","applicationId":"desktop","name":"Desktop",
                "assignments":[{"slotId":"g915_input_configuration","cardId":"config"}]
            }]},
            "cards":{"cards":[
                {"id":"config","attribute":"INPUT_CONFIGURATION",
                 "inputConfiguration":{"layerPresets":{"10":"preset","11":"preset-shift"}}},
                {"id":"preset","attribute":"INPUT_PRESET","inputPreset":{"assignments":{"1":{
                    "category":"G_KEY","inputAssignments":{
                        "1":{"inputId":1,"exclusiveAssignment":"GSHIFT"},
                        "2":{"inputId":2,"exclusiveAssignment":"NONE","modifierAssignments":{"0":{
                            "eventAssignments":{"1":{"macroId":"0f82f693-5b78-4cf5-867e-013a00000000"}}
                        }}}
                    }
                }}}}
                ,{"id":"preset-shift","attribute":"INPUT_PRESET","inputPreset":{"assignments":{"1":{
                    "category":"G_KEY","inputAssignments":{
                        "2":{"inputId":2,"modifierAssignments":{"0":{
                            "eventAssignments":{"1":{"macroId":"0f82f693-5b78-4cf5-867e-013b00000000"}}
                        }}}
                    }
                }}}}
            ]},
            "applications":{"applications":[]}
        });
        let imported = import_ghub_json(&document.to_string(), None).unwrap();
        let bindings = &imported.profiles[0].bindings;
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].slot_id, "g915_g1_m1");
        assert_eq!(bindings[0].source_action, "device:G_SHIFT");
        assert_eq!(bindings[0].onboard_binding, "g-shift");
        assert_eq!(bindings[1].onboard_binding, "key:f1");
        assert_eq!(bindings[2].onboard_binding, "key:f2");
        assert!(
            imported
                .summary
                .unmapped_classes
                .contains_key("daemon_gshift_unsupported")
        );
        assert_eq!(
            imported.bindings.devices["g915"].gkeys["g2"],
            Action::Keys(KeysAction { keys: "f1".into() })
        );
    }

    #[test]
    fn full_fixture_import_is_deterministic_and_does_not_use_sqlite() {
        let first = import_ghub_json(FIXTURE, None).unwrap();
        let second = import_ghub_json(FIXTURE, None).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.profiles.len(), 1);
        let profile = &first.profiles[0];
        assert_eq!(profile.name, "Desktop");
        assert_eq!(profile.default_dpi, 4000);
        assert_eq!(profile.bindings.len(), 4);
        assert_eq!(profile.bindings[0].slot_id, "g502x-lightspeed_g5_m1");
        assert_eq!(profile.macros.len(), 1);
        assert_eq!(profile.lighting.len(), 1);
        assert_eq!(profile.lighting[0].persist, "nvm");
        assert_eq!(first.summary.cards, 3);
        assert_eq!(first.summary.lighting_cards, 1);
        assert_eq!(output_paths(&first, Path::new("out")).len(), 4);
        assert_eq!(
            import_ghub_json(FIXTURE, Some("g502x-lightspeed"))
                .unwrap()
                .profiles,
            first.profiles
        );
        assert_eq!(
            serde_json::to_string_pretty(&first.profiles).unwrap(),
            serde_json::to_string_pretty(&second.profiles).unwrap()
        );
    }
}

mod bindings;
mod daemon;
mod device_data;
mod discovery;
mod ghub_import;
mod gkeys;
mod hidpp;
mod lighting;
mod listener;
mod mkeys;
mod onboard;
mod output;
mod profile;
mod specialkeys;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use device_data::DeviceRecord;
use discovery::{Discovery, ManagedDevice, discover, error_text};
use ghub_import::{
    ImportResult, default_ghub_db_path, import_ghub_database, output_paths, save_import,
};
use gkeys::GKeys;
use hidpp::device::{BatteryStatus, FeatureInfo};
use lighting::brightness::{
    Brightness, BrightnessInfo, percent_from_raw as brightness_percent,
    raw_from_percent as brightness_raw,
};
use lighting::perkey::{
    PerKeyLightingV2, ResolvedKey, ZoneScheme, probe_zones, resolve_key, zones_from_usages,
};
use lighting::rgb::{
    Effect, EffectOptions, Persistence, RgbCapabilities, RgbColor, RgbEffects, encode_effect,
    parse_direction,
};
use onboard::{
    Binding as OnboardBinding, ButtonRow, Description as OnboardDescription, DirectoryEntry, Macro,
    Onboard, SectorDiff, VerificationMethod, backup_path, button_rows, decode_macro, encode_dump,
    export_state, first_enabled_sector, import_plan, led_slots, load_dump, load_export,
    macro_sector_ids, parse_binding, profile_name, repack_export_macros, require_backup, save_dump,
    save_export, set_button as set_onboard_button, set_dpi as set_onboard_dpi, set_led_slot,
    set_profile_name, set_rate as set_onboard_rate,
};
use output::{print_json, print_table};
use profile::{
    Profile, RgbPreset, apply_to_onboard_export, default_output_dir, default_store_path,
    load_portable_onboard, load_store,
};
use specialkeys::{CidReporting, ReportingUpdate, SpecialKeys, ensure_can_remap, resolve_cid};

#[derive(Debug, Parser)]
#[command(name = "logihub", version, about = "Lightweight Logitech HID++ CLI")]
struct Cli {
    #[arg(long, global = true, help = "Emit JSON instead of a table")]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List receivers and paired/direct devices.
    List,
    /// Show the embedded model record and live HID++ features.
    DeviceInfo {
        #[arg(long)]
        device: Option<usize>,
    },
    /// Read battery state from one or all devices.
    Battery {
        #[arg(long)]
        device: Option<usize>,
    },
    /// Get or set sensor DPI.
    Dpi {
        #[command(subcommand)]
        command: DpiCommand,
    },
    /// Get or set report rate.
    Rate {
        #[command(subcommand)]
        command: RateCommand,
    },
    /// Dump the HID++ 2.0 feature table.
    Features {
        #[arg(long)]
        device: Option<usize>,
    },
    /// Import, list, or apply saved profiles.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Inspect and edit onboard profile memory.
    Onboard {
        #[command(subcommand)]
        command: OnboardCommand,
    },
    /// Inspect and edit onboard button bindings.
    Buttons {
        #[command(subcommand)]
        command: ButtonsCommand,
    },
    /// Inspect or set keyboard lighting brightness (0x8040).
    Brightness {
        #[command(subcommand)]
        command: Option<BrightnessCommand>,
        #[arg(long, global = true)]
        device: Option<usize>,
    },
    /// Inspect or set firmware RGB effects (0x8071).
    Rgb {
        #[command(subcommand)]
        command: RgbCommand,
        #[arg(long, global = true)]
        device: Option<usize>,
    },
    /// Send per-key RGB frames (0x8081).
    Perkey {
        #[command(subcommand)]
        command: PerKeyCommand,
        #[arg(long, global = true)]
        device: Option<usize>,
        #[arg(long, global = true, value_enum)]
        zone_scheme: Option<ZoneSchemeArg>,
    },
    /// Inspect or divert dedicated G-keys (0x8010).
    Gkeys {
        #[command(subcommand)]
        command: GKeysCommand,
    },
    /// Set M1/M2/M3 indicator LEDs (0x8020).
    Mkeys {
        #[command(subcommand)]
        command: MKeysCommand,
    },
    /// Set the macro-record indicator LED (0x8030).
    Mr {
        state: OnOff,
        #[arg(long)]
        device: Option<usize>,
    },
    /// Inspect, divert, remap, or reset reprogrammable controls (0x1B04).
    Keys {
        #[command(subcommand)]
        command: KeysCommand,
    },
    /// Print decoded HID++ notifications until Ctrl+C.
    Watch {
        #[arg(long)]
        device: Option<usize>,
    },
    /// Run configured G-key and special-key bindings until stopped.
    Daemon {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        verbose: bool,
    },
}

#[derive(Debug, Subcommand)]
enum GKeysCommand {
    Info {
        #[arg(long)]
        device: Option<usize>,
    },
    SoftwareMode {
        state: OnOff,
        #[arg(long)]
        device: Option<usize>,
    },
}

#[derive(Debug, Subcommand)]
enum MKeysCommand {
    Set {
        key: MKeySelection,
        #[arg(long)]
        device: Option<usize>,
    },
}

#[derive(Debug, Subcommand)]
enum KeysCommand {
    List {
        #[arg(long)]
        device: Option<usize>,
    },
    Divert {
        cid: String,
        state: OnOff,
        #[arg(long)]
        persist: bool,
        #[arg(long, value_enum)]
        raw_xy: Option<OnOff>,
        #[arg(long)]
        device: Option<usize>,
    },
    Remap {
        #[arg(value_parser = parse_u16_arg)]
        cid: u16,
        #[arg(value_parser = parse_u16_arg)]
        target_cid: u16,
        #[arg(long)]
        device: Option<usize>,
    },
    Reset {
        #[arg(long)]
        device: Option<usize>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

impl OnOff {
    fn enabled(self) -> bool {
        matches!(self, Self::On)
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum MKeySelection {
    M1,
    M2,
    M3,
    None,
}

impl MKeySelection {
    fn mask(self) -> u8 {
        match self {
            Self::M1 => 0x01,
            Self::M2 => 0x02,
            Self::M3 => 0x04,
            Self::None => 0,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::M1 => "m1",
            Self::M2 => "m2",
            Self::M3 => "m3",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Subcommand)]
enum BrightnessCommand {
    /// Set a percentage, or use `set raw N` for a device-native value.
    Set {
        value: String,
        raw_value: Option<u16>,
    },
}

#[derive(Debug, Subcommand)]
enum RgbCommand {
    Info,
    Set {
        #[arg(long)]
        zone: String,
        #[arg(long)]
        effect: String,
        #[arg(long)]
        color: Option<String>,
        #[arg(long)]
        color2: Option<String>,
        #[arg(long)]
        speed: Option<u16>,
        #[arg(long)]
        period: Option<u16>,
        #[arg(long)]
        brightness: Option<u8>,
        #[arg(long)]
        intensity: Option<u8>,
        #[arg(long)]
        direction: Option<String>,
        #[arg(long, value_enum, default_value_t = PersistArg::Ram)]
        persist: PersistArg,
    },
    Off {
        #[arg(long, default_value = "all")]
        zone: String,
    },
    Power {
        #[command(subcommand)]
        command: Option<RgbPowerCommand>,
    },
    Nv {
        #[command(subcommand)]
        command: RgbNvCommand,
    },
}

#[derive(Debug, Subcommand)]
enum RgbPowerCommand {
    Get,
    Set {
        #[arg(value_parser = parse_u8_arg)]
        mode: u8,
    },
}

#[derive(Debug, Subcommand)]
enum RgbNvCommand {
    Get {
        #[arg(value_parser = parse_u16_arg)]
        item: u16,
    },
    Set {
        #[arg(value_parser = parse_u16_arg)]
        item: u16,
        #[arg(required = true, num_args = 1..=7)]
        value: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum PerKeyCommand {
    Probe,
    Set {
        #[arg(required = true)]
        assignments: Vec<String>,
        #[arg(long)]
        persist: bool,
    },
    Fill {
        color: String,
        #[arg(long)]
        persist: bool,
    },
    Clear {
        #[arg(long)]
        persist: bool,
    },
    Frame {
        #[arg(long = "from")]
        from: PathBuf,
        #[arg(long)]
        persist: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PersistArg {
    Ram,
    Nvm,
    Powersave,
}

impl From<PersistArg> for Persistence {
    fn from(value: PersistArg) -> Self {
        match value {
            PersistArg::Ram => Self::Ram,
            PersistArg::Nvm => Self::Nvm,
            PersistArg::Powersave => Self::PowerSave,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ZoneSchemeArg {
    Hidusage,
    Solaar,
}

impl From<ZoneSchemeArg> for ZoneScheme {
    fn from(value: ZoneSchemeArg) -> Self {
        match value {
            ZoneSchemeArg::Hidusage => Self::HidUsage,
            ZoneSchemeArg::Solaar => Self::Solaar,
        }
    }
}

#[derive(Debug, Subcommand)]
enum DpiCommand {
    Get {
        #[arg(long)]
        device: Option<usize>,
    },
    Set {
        value: u16,
        #[arg(long)]
        device: Option<usize>,
    },
}

#[derive(Debug, Subcommand)]
enum RateCommand {
    Get {
        #[arg(long)]
        device: Option<usize>,
    },
    Set {
        hz: u32,
        #[arg(long)]
        device: Option<usize>,
    },
}

#[derive(Debug, Subcommand)]
enum ProfileCommand {
    /// Import profiles, assignments, macros, and lighting from G HUB.
    ImportGhub {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long)]
        out_dir: Option<PathBuf>,
        #[arg(long)]
        device_model: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// List saved profiles.
    List,
    /// Show all details for one profile, or every profile when omitted.
    Show { name: Option<String> },
    /// Apply a profile live, or import it into onboard memory with --onboard.
    Apply {
        name: String,
        #[arg(long)]
        device: Option<usize>,
        #[arg(long)]
        onboard: bool,
        #[arg(long, requires = "onboard")]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum OnboardCommand {
    Info {
        #[arg(long)]
        device: Option<usize>,
    },
    Dump {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        device: Option<usize>,
    },
    /// Export the complete onboard state as editable JSON.
    Export {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        device: Option<usize>,
    },
    /// Import onboard JSON after showing a sector diff.
    Import {
        #[arg(long = "in")]
        input: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, conflicts_with = "dry_run")]
        yes: bool,
        #[arg(long)]
        device: Option<usize>,
    },
    GetName {
        #[arg(long)]
        device: Option<usize>,
    },
    SetName {
        name: String,
        #[arg(long)]
        device: Option<usize>,
    },
    Crc {
        #[arg(value_parser = parse_u16_arg)]
        sector: u16,
        #[arg(long)]
        device: Option<usize>,
    },
    ExecMacro {
        #[arg(value_parser = parse_u16_arg)]
        sector: u16,
        #[arg(value_parser = parse_u16_arg)]
        offset: u16,
        #[arg(long)]
        device: Option<usize>,
    },
    Restore {
        #[arg(long = "in")]
        input: PathBuf,
        #[arg(long)]
        device: Option<usize>,
    },
    SetDpi {
        #[arg(required = true, num_args = 1..=5)]
        levels: Vec<u16>,
        #[arg(long)]
        default: usize,
        #[arg(long)]
        shift: Option<u16>,
        #[arg(long)]
        device: Option<usize>,
    },
    SetRate {
        hz: u32,
        #[arg(long)]
        device: Option<usize>,
    },
    /// Set the active onboard DPI slot (0-based; persists across sleep).
    SetDpiIndex {
        index: u8,
        #[arg(long)]
        device: Option<usize>,
    },
    /// Set a normal or G-Shift button-table entry.
    SetButton {
        n: usize,
        binding: String,
        #[arg(long)]
        gshift: bool,
        #[arg(long)]
        device: Option<usize>,
    },
    /// Keyboard-flavoured alias of set-button.
    SetGkey {
        n: usize,
        binding: String,
        #[arg(long)]
        gshift: bool,
        #[arg(long)]
        device: Option<usize>,
    },
    /// Inspect or edit onboard macro bytecode.
    Macro {
        #[command(subcommand)]
        command: OnboardMacroCommand,
    },
    /// Inspect or edit the four startup LED slots in the active profile.
    Led {
        #[command(subcommand)]
        command: OnboardLedCommand,
    },
    Mode {
        #[command(subcommand)]
        command: OnboardModeCommand,
    },
}

#[derive(Debug, Subcommand)]
enum OnboardMacroCommand {
    List {
        #[arg(long)]
        device: Option<usize>,
    },
    Show {
        #[arg(value_parser = parse_u16_arg)]
        sector: u16,
        #[arg(value_parser = parse_u16_arg)]
        offset: u16,
        #[arg(long)]
        device: Option<usize>,
    },
    Set {
        n: usize,
        #[arg(long)]
        gshift: bool,
        #[arg(long)]
        steps: String,
        #[arg(long)]
        device: Option<usize>,
    },
    Clear {
        n: usize,
        #[arg(long)]
        gshift: bool,
        #[arg(long)]
        device: Option<usize>,
    },
}

#[derive(Debug, Subcommand)]
enum OnboardLedCommand {
    Show {
        #[arg(long)]
        device: Option<usize>,
    },
    Set {
        slot: usize,
        #[arg(long)]
        effect: String,
        #[arg(long)]
        color: Option<String>,
        #[arg(long)]
        color2: Option<String>,
        #[arg(long)]
        speed: Option<u16>,
        #[arg(long)]
        period: Option<u16>,
        #[arg(long)]
        brightness: Option<u8>,
        #[arg(long)]
        intensity: Option<u8>,
        #[arg(long)]
        direction: Option<String>,
        #[arg(long)]
        device: Option<usize>,
    },
}

#[derive(Debug, Subcommand)]
enum OnboardModeCommand {
    Get {
        #[arg(long)]
        device: Option<usize>,
    },
    Set {
        state: OnOff,
        #[arg(long)]
        device: Option<usize>,
    },
    /// Legacy alias for `mode set on`.
    Onboard {
        #[arg(long)]
        device: Option<usize>,
    },
    /// Legacy alias for `mode set off`.
    Host {
        #[arg(long)]
        device: Option<usize>,
    },
}

#[derive(Debug, Subcommand)]
enum ButtonsCommand {
    List {
        #[arg(long)]
        device: Option<usize>,
    },
    Set {
        n: usize,
        binding: String,
        #[arg(long)]
        gshift: bool,
        #[arg(long)]
        device: Option<usize>,
    },
}

#[derive(Serialize)]
struct BatteryResult {
    device: usize,
    name: String,
    percent: Option<u8>,
    status: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct ValueResult<T: Serialize> {
    device: usize,
    name: String,
    value: Option<T>,
    error: Option<String>,
}

#[derive(Serialize)]
struct SetResult<T: Serialize> {
    device: usize,
    name: String,
    value: T,
    status: String,
}

#[derive(Serialize)]
struct FeatureResult {
    device: usize,
    name: String,
    features: Option<Vec<FeatureInfo>>,
    error: Option<String>,
}

#[derive(Serialize)]
struct DeviceInfoResult<'a> {
    device: usize,
    name: String,
    model: Option<&'a DeviceRecord>,
    features: Option<Vec<FeatureInfo>>,
    error: Option<String>,
}

#[derive(Serialize)]
struct ProfileApplyResult {
    profile: String,
    device: usize,
    device_name: String,
    active_dpi: u16,
    report_rate_hz: u32,
    lighting_effects: usize,
    mode: String,
}

#[derive(Serialize)]
struct OnboardInfoResult {
    device: usize,
    name: String,
    description: OnboardDescription,
    mode: String,
    current_profile: u16,
    current_dpi_index: u8,
    directory: Vec<DirectoryEntry>,
    directory_raw: String,
}

#[derive(Serialize)]
struct OnboardNameResult {
    device: usize,
    name: String,
    sector: u16,
    profile_name: Option<String>,
}

#[derive(Serialize)]
struct OnboardCrcResult {
    device: usize,
    name: String,
    sector: u16,
    crc: u16,
    raw: [u8; 16],
}

#[derive(Serialize)]
struct OnboardWriteResult {
    device: usize,
    name: String,
    operation: String,
    status: String,
}

#[derive(Serialize)]
struct BrightnessResult {
    device: usize,
    name: String,
    info: BrightnessInfo,
    raw: u16,
    percent: u8,
    illumination: Option<bool>,
}

#[derive(Serialize)]
struct BrightnessSetResult {
    device: usize,
    name: String,
    requested_raw: u16,
    effective_raw: u16,
    percent: u8,
    status: String,
}

#[derive(Serialize)]
struct RgbInfoResult {
    device: usize,
    name: String,
    capabilities: RgbCapabilities,
}

#[derive(Serialize)]
struct RgbWriteResult {
    device: usize,
    name: String,
    effect: String,
    zones: Vec<u8>,
    persistence: String,
    status: String,
}

#[derive(Serialize)]
struct RgbPowerResult {
    device: usize,
    name: String,
    mode: u8,
    status: String,
}

#[derive(Serialize)]
struct RgbNvResult {
    device: usize,
    name: String,
    item: u16,
    value: String,
    status: String,
}

#[derive(Serialize)]
struct PerKeyWriteResult {
    device: usize,
    name: String,
    zone_scheme: ZoneScheme,
    keys: Vec<ResolvedKey>,
    zone_count: usize,
    requests: usize,
    persistent: bool,
    status: String,
}

#[derive(Serialize)]
struct PerKeyProbeResult {
    device: usize,
    name: String,
    hidusage_expected: String,
    solaar_expected: String,
    answer: String,
    status: String,
}

#[derive(Serialize)]
struct GKeysInfoResult {
    device: usize,
    name: String,
    count: u8,
    physical_layout: u16,
    physical_layout_hex: String,
}

#[derive(Serialize)]
struct FeatureWriteResult {
    device: usize,
    name: String,
    feature: String,
    value: String,
    status: String,
}

#[derive(Serialize)]
struct SpecialKeyRow {
    device: usize,
    index: u8,
    cid: u16,
    cid_hex: String,
    name: String,
    task_id: u16,
    task_hex: String,
    flags_raw: u8,
    additional_flags_raw: u8,
    flags: Vec<String>,
    position: u8,
    group: u8,
    group_mask: u8,
    reporting: Option<CidReporting>,
    reporting_error: Option<String>,
}

#[derive(Serialize)]
struct SpecialKeysListResult {
    device: usize,
    name: String,
    capabilities: u8,
    keys: Vec<SpecialKeyRow>,
}

#[derive(Serialize)]
struct SpecialKeyWriteResult {
    device: usize,
    name: String,
    cid: Option<u16>,
    operation: String,
    reporting: Option<CidReporting>,
    status: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::List => list(&discover_with_warnings()?, cli.json),
        Command::DeviceInfo { device } => device_info(&discover_with_warnings()?, device, cli.json),
        Command::Battery { device } => battery(&discover_with_warnings()?, device, cli.json),
        Command::Dpi { command } => {
            let discovery = discover_with_warnings()?;
            match command {
                DpiCommand::Get { device } => dpi_get(&discovery, device, cli.json),
                DpiCommand::Set { value, device } => dpi_set(&discovery, device, value, cli.json),
            }
        }
        Command::Rate { command } => {
            let discovery = discover_with_warnings()?;
            match command {
                RateCommand::Get { device } => rate_get(&discovery, device, cli.json),
                RateCommand::Set { hz, device } => rate_set(&discovery, device, hz, cli.json),
            }
        }
        Command::Features { device } => features(&discover_with_warnings()?, device, cli.json),
        Command::Profile { command } => match command {
            ProfileCommand::ImportGhub {
                db,
                out_dir,
                device_model,
                dry_run,
            } => profile_import_ghub(db, out_dir, device_model.as_deref(), dry_run, cli.json),
            ProfileCommand::List => profile_list(cli.json),
            ProfileCommand::Show { name } => profile_show(name.as_deref(), cli.json),
            ProfileCommand::Apply {
                name,
                device,
                onboard,
                yes,
            } => profile_apply(&name, device, onboard, yes, cli.json),
        },
        Command::Onboard { command } => match command {
            OnboardCommand::Info { device } => onboard_info(device, cli.json),
            OnboardCommand::Dump { out, device } => onboard_dump(&out, device, cli.json),
            OnboardCommand::Export { out, device } => onboard_export(&out, device, cli.json),
            OnboardCommand::Import {
                input,
                dry_run,
                yes,
                device,
            } => onboard_import(&input, dry_run, yes, device, cli.json),
            OnboardCommand::GetName { device } => onboard_get_name(device, cli.json),
            OnboardCommand::SetName { name, device } => onboard_set_name(&name, device, cli.json),
            OnboardCommand::Crc { sector, device } => onboard_crc(sector, device, cli.json),
            OnboardCommand::ExecMacro {
                sector,
                offset,
                device,
            } => onboard_exec_macro(sector, offset, device, cli.json),
            OnboardCommand::Restore { input, device } => onboard_restore(&input, device, cli.json),
            OnboardCommand::SetDpi {
                levels,
                default,
                shift,
                device,
            } => onboard_set_dpi(&levels, default, shift, device, cli.json),
            OnboardCommand::SetRate { hz, device } => onboard_set_rate(hz, device, cli.json),
            OnboardCommand::SetDpiIndex { index, device } => {
                onboard_set_dpi_index(index, device, cli.json)
            }
            OnboardCommand::SetButton {
                n,
                binding,
                gshift,
                device,
            } => buttons_set(n, &binding, gshift, device, cli.json, false),
            OnboardCommand::SetGkey {
                n,
                binding,
                gshift,
                device,
            } => buttons_set(n, &binding, gshift, device, cli.json, true),
            OnboardCommand::Macro { command } => match command {
                OnboardMacroCommand::List { device } => onboard_macro_list(device, cli.json),
                OnboardMacroCommand::Show {
                    sector,
                    offset,
                    device,
                } => onboard_macro_show(sector, offset, device, cli.json),
                OnboardMacroCommand::Set {
                    n,
                    gshift,
                    steps,
                    device,
                } => onboard_macro_set(n, gshift, &steps, device, cli.json),
                OnboardMacroCommand::Clear { n, gshift, device } => {
                    onboard_macro_clear(n, gshift, device, cli.json)
                }
            },
            OnboardCommand::Led { command } => match command {
                OnboardLedCommand::Show { device } => onboard_led_show(device, cli.json),
                OnboardLedCommand::Set {
                    slot,
                    effect,
                    color,
                    color2,
                    speed,
                    period,
                    brightness,
                    intensity,
                    direction,
                    device,
                } => onboard_led_set(
                    slot,
                    &effect,
                    color.as_deref(),
                    color2.as_deref(),
                    speed,
                    period,
                    brightness,
                    intensity,
                    direction.as_deref(),
                    device,
                    cli.json,
                ),
            },
            OnboardCommand::Mode { command } => match command {
                OnboardModeCommand::Get { device } => onboard_mode_get(device, cli.json),
                OnboardModeCommand::Set { state, device } => {
                    onboard_mode_set(state.enabled(), device, cli.json)
                }
                OnboardModeCommand::Onboard { device } => onboard_mode_set(true, device, cli.json),
                OnboardModeCommand::Host { device } => onboard_mode_set(false, device, cli.json),
            },
        },
        Command::Buttons { command } => match command {
            ButtonsCommand::List { device } => buttons_list(device, cli.json),
            ButtonsCommand::Set {
                n,
                binding,
                gshift,
                device,
            } => buttons_set(n, &binding, gshift, device, cli.json, false),
        },
        Command::Brightness { command, device } => {
            let discovery = discover_with_warnings()?;
            match command {
                None => brightness_get(&discovery, device, cli.json),
                Some(BrightnessCommand::Set { value, raw_value }) => {
                    brightness_set(&discovery, device, &value, raw_value, cli.json)
                }
            }
        }
        Command::Rgb { command, device } => {
            let discovery = discover_with_warnings()?;
            match command {
                RgbCommand::Info => rgb_info(&discovery, device, cli.json),
                RgbCommand::Set {
                    zone,
                    effect,
                    color,
                    color2,
                    speed,
                    period,
                    brightness,
                    intensity,
                    direction,
                    persist,
                } => rgb_set(
                    &discovery,
                    device,
                    &zone,
                    &effect,
                    color.as_deref(),
                    color2.as_deref(),
                    speed,
                    period,
                    brightness,
                    intensity,
                    direction.as_deref(),
                    persist.into(),
                    cli.json,
                ),
                RgbCommand::Off { zone } => rgb_set(
                    &discovery,
                    device,
                    &zone,
                    "off",
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Persistence::Ram,
                    cli.json,
                ),
                RgbCommand::Power { command } => match command {
                    None | Some(RgbPowerCommand::Get) => {
                        rgb_power(&discovery, device, None, cli.json)
                    }
                    Some(RgbPowerCommand::Set { mode }) => {
                        rgb_power(&discovery, device, Some(mode), cli.json)
                    }
                },
                RgbCommand::Nv { command } => match command {
                    RgbNvCommand::Get { item } => rgb_nv_get(&discovery, device, item, cli.json),
                    RgbNvCommand::Set { item, value } => {
                        rgb_nv_set(&discovery, device, item, &value, cli.json)
                    }
                },
            }
        }
        Command::Perkey {
            command,
            device,
            zone_scheme,
        } => {
            let discovery = discover_with_warnings()?;
            let scheme = zone_scheme.map(Into::into);
            match command {
                PerKeyCommand::Probe => perkey_probe(&discovery, device, cli.json),
                PerKeyCommand::Set {
                    assignments,
                    persist,
                } => perkey_set(&discovery, device, scheme, &assignments, persist, cli.json),
                PerKeyCommand::Fill { color, persist } => {
                    perkey_fill(&discovery, device, scheme, &color, persist, cli.json)
                }
                PerKeyCommand::Clear { persist } => {
                    perkey_fill(&discovery, device, scheme, "000000", persist, cli.json)
                }
                PerKeyCommand::Frame { from, persist } => {
                    perkey_frame(&discovery, device, scheme, &from, persist, cli.json)
                }
            }
        }
        Command::Gkeys { command } => {
            let discovery = discover_with_warnings()?;
            match command {
                GKeysCommand::Info { device } => gkeys_info(&discovery, device, cli.json),
                GKeysCommand::SoftwareMode { state, device } => {
                    gkeys_software_mode(&discovery, device, state.enabled(), cli.json)
                }
            }
        }
        Command::Mkeys { command } => {
            let discovery = discover_with_warnings()?;
            match command {
                MKeysCommand::Set { key, device } => mkeys_set(&discovery, device, key, cli.json),
            }
        }
        Command::Mr { state, device } => {
            let discovery = discover_with_warnings()?;
            mr_set(&discovery, device, state.enabled(), cli.json)
        }
        Command::Keys { command } => {
            let discovery = discover_with_warnings()?;
            match command {
                KeysCommand::List { device } => keys_list(&discovery, device, cli.json),
                KeysCommand::Divert {
                    cid,
                    state,
                    persist,
                    raw_xy,
                    device,
                } => keys_divert(
                    &discovery,
                    device,
                    &cid,
                    state.enabled(),
                    persist,
                    raw_xy.map(OnOff::enabled),
                    cli.json,
                ),
                KeysCommand::Remap {
                    cid,
                    target_cid,
                    device,
                } => keys_remap(&discovery, device, cid, target_cid, cli.json),
                KeysCommand::Reset { device } => keys_reset(&discovery, device, cli.json),
            }
        }
        Command::Watch { device } => {
            let discovery = discover_with_warnings()?;
            daemon::watch(&discovery, device, cli.json)
        }
        Command::Daemon { config, verbose } => daemon::run(config, verbose, cli.json),
    }
}

fn discover_with_warnings() -> Result<Discovery> {
    let discovery = discover()?;
    for warning in &discovery.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(discovery)
}

fn list(discovery: &Discovery, json: bool) -> Result<()> {
    if json {
        return print_json(&discovery.rows);
    }
    let rows = discovery
        .rows
        .iter()
        .map(|row| {
            vec![
                row.index.to_string(),
                row.kind.clone(),
                row.name.clone(),
                row.wireless_pid
                    .map(|pid| format!("0x{pid:04X}"))
                    .unwrap_or_else(|| "-".into()),
                row.model_id.clone().unwrap_or_else(|| "-".into()),
                row.display_name.clone().unwrap_or_else(|| "-".into()),
                row.status.clone(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &[
            "INDEX",
            "TYPE",
            "NAME",
            "WIRELESS PID",
            "MODEL ID",
            "DISPLAY NAME",
            "STATUS",
        ],
        &rows,
    );
    Ok(())
}

fn device_info(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let target = single_device(discovery, index)?;
    let (features, error) = match target.device.features() {
        Ok(features) => (Some(features), None),
        Err(error) => (None, Some(error_text(error))),
    };
    let result = DeviceInfoResult {
        device: target.index,
        name: target.name.clone(),
        model: target.model,
        features,
        error,
    };
    if json {
        return print_json(&result);
    }

    print_table(
        &["DEVICE", "NAME", "MODEL ID", "DISPLAY NAME", "TYPE"],
        &[vec![
            result.device.to_string(),
            result.name.clone(),
            result
                .model
                .map(|model| model.model_id.clone())
                .unwrap_or_else(|| "-".into()),
            result
                .model
                .map(|model| model.display_name.clone())
                .unwrap_or_else(|| "-".into()),
            result
                .model
                .map(|model| model.kind.clone())
                .unwrap_or_else(|| "-".into()),
        ]],
    );
    if let Some(model) = result.model {
        let dpi = model
            .dpi_default
            .as_ref()
            .map(|dpi| {
                format!(
                    "levels={:?}, default={}, shift={}",
                    dpi.levels, dpi.default, dpi.shift
                )
            })
            .unwrap_or_else(|| "-".into());
        let rows = vec![
            vec!["pids".into(), model.pids.join(", ")],
            vec![
                "slot_prefix".into(),
                model.slot_prefix.clone().unwrap_or_else(|| "-".into()),
            ],
            vec![
                "lighting.category".into(),
                model
                    .lighting
                    .category
                    .clone()
                    .unwrap_or_else(|| "-".into()),
            ],
            vec![
                "lighting.per_key".into(),
                model.lighting.per_key.to_string(),
            ],
            vec![
                "lighting.persistence".into(),
                serde_json::to_string(&model.lighting.persistence)?,
            ],
            vec!["input.categories".into(), model.input.categories.join(", ")],
            vec!["input.layers".into(), model.input.layers.join(", ")],
            vec![
                "gkeys.count".into(),
                model
                    .gkeys
                    .count
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "-".into()),
            ],
            vec![
                "onboard.supported".into(),
                model.onboard.supported.to_string(),
            ],
            vec!["dpi_default".into(), dpi],
            vec![
                "per_key_map".into(),
                model
                    .per_key_map
                    .as_ref()
                    .map(|map| format!("{} HID usages", map.entries.len()))
                    .unwrap_or_else(|| "-".into()),
            ],
        ];
        print_table(&["MODEL FIELD", "VALUE"], &rows);
        let zones = model
            .lighting
            .zones
            .iter()
            .map(|zone| vec![zone.zone_type.clone(), zone.effects.join(", ")])
            .collect::<Vec<_>>();
        print_table(&["LIGHTING ZONE", "EFFECTS"], &zones);
    } else {
        println!("MODEL RECORD: unknown PID");
    }
    let rows = result
        .features
        .unwrap_or_default()
        .into_iter()
        .map(|feature| {
            vec![
                feature.index.to_string(),
                format!("0x{:04X}", feature.id),
                feature.name.into(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["INDEX", "ID", "FEATURE"], &rows);
    if let Some(error) = result.error {
        eprintln!("feature read failed: {error}");
    }
    Ok(())
}

fn battery(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let devices = selected_devices(discovery, index)?;
    let results = devices
        .into_iter()
        .map(|target| match target.device.battery() {
            Ok(BatteryStatus { percent, status }) => BatteryResult {
                device: target.index,
                name: target.name.clone(),
                percent: Some(percent),
                status: Some(status),
                error: None,
            },
            Err(error) => BatteryResult {
                device: target.index,
                name: target.name.clone(),
                percent: None,
                status: None,
                error: Some(error_text(error)),
            },
        })
        .collect::<Vec<_>>();
    if json {
        return print_json(&results);
    }
    let rows = results
        .iter()
        .map(|result| {
            vec![
                result.device.to_string(),
                result.name.clone(),
                result
                    .percent
                    .map(|value| format!("{value}%"))
                    .unwrap_or_else(|| "-".into()),
                result.status.clone().unwrap_or_else(|| "取得失敗".into()),
                result.error.clone().unwrap_or_default(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["DEVICE", "NAME", "BATTERY", "STATUS", "ERROR"], &rows);
    Ok(())
}

fn dpi_get(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let devices = selected_devices(discovery, index)?;
    let results = devices
        .into_iter()
        .map(|target| match target.device.dpi() {
            Ok(value) => ValueResult {
                device: target.index,
                name: target.name.clone(),
                value: Some(value),
                error: None,
            },
            Err(error) => ValueResult {
                device: target.index,
                name: target.name.clone(),
                value: None,
                error: Some(error_text(error)),
            },
        })
        .collect::<Vec<_>>();
    print_value_results(&results, "DPI", json)
}

fn dpi_set(discovery: &Discovery, index: Option<usize>, value: u16, json: bool) -> Result<()> {
    let target = single_device(discovery, index)?;
    target.device.set_dpi(value).map_err(anyhow::Error::new)?;
    let result = SetResult {
        device: target.index,
        name: target.name.clone(),
        value,
        status: "set".into(),
    };
    if json {
        print_json(&result)
    } else {
        print_table(
            &["DEVICE", "NAME", "DPI", "STATUS"],
            &[vec![
                result.device.to_string(),
                result.name,
                result.value.to_string(),
                result.status,
            ]],
        );
        Ok(())
    }
}

fn rate_get(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let devices = selected_devices(discovery, index)?;
    let results = devices
        .into_iter()
        .map(|target| match target.device.report_rate() {
            Ok(value) => ValueResult {
                device: target.index,
                name: target.name.clone(),
                value: Some(value),
                error: None,
            },
            Err(error) => ValueResult {
                device: target.index,
                name: target.name.clone(),
                value: None,
                error: Some(error_text(error)),
            },
        })
        .collect::<Vec<_>>();
    print_value_results(&results, "HZ", json)
}

fn rate_set(discovery: &Discovery, index: Option<usize>, hz: u32, json: bool) -> Result<()> {
    let target = single_device(discovery, index)?;
    target
        .device
        .set_report_rate(hz)
        .map_err(anyhow::Error::new)?;
    let result = SetResult {
        device: target.index,
        name: target.name.clone(),
        value: hz,
        status: "set".into(),
    };
    if json {
        print_json(&result)
    } else {
        print_table(
            &["DEVICE", "NAME", "HZ", "STATUS"],
            &[vec![
                result.device.to_string(),
                result.name,
                result.value.to_string(),
                result.status,
            ]],
        );
        Ok(())
    }
}

fn features(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let devices = selected_devices(discovery, index)?;
    let results = devices
        .into_iter()
        .map(|target| match target.device.features() {
            Ok(features) => FeatureResult {
                device: target.index,
                name: target.name.clone(),
                features: Some(features),
                error: None,
            },
            Err(error) => FeatureResult {
                device: target.index,
                name: target.name.clone(),
                features: None,
                error: Some(error_text(error)),
            },
        })
        .collect::<Vec<_>>();
    if json {
        return print_json(&results);
    }
    let mut rows = Vec::new();
    for result in results {
        if let Some(features) = result.features {
            for feature in features {
                rows.push(vec![
                    result.device.to_string(),
                    result.name.clone(),
                    feature.index.to_string(),
                    format!("0x{:04X}", feature.id),
                    feature.name.into(),
                    String::new(),
                ]);
            }
        } else {
            rows.push(vec![
                result.device.to_string(),
                result.name,
                "-".into(),
                "-".into(),
                "取得失敗".into(),
                result.error.unwrap_or_default(),
            ]);
        }
    }
    print_table(
        &["DEVICE", "NAME", "INDEX", "ID", "FEATURE", "ERROR"],
        &rows,
    );
    Ok(())
}

fn gkeys_info(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let target = single_device(discovery, index)?;
    let gkeys = GKeys::new(&target.device)?;
    let physical_layout = gkeys.get_physical_layout()?;
    let result = GKeysInfoResult {
        device: target.index,
        name: target.name.clone(),
        count: gkeys.get_count()?,
        physical_layout,
        physical_layout_hex: format!("0x{physical_layout:04X}"),
    };
    if json {
        return print_json(&result);
    }
    print_table(
        &["DEVICE", "NAME", "COUNT", "PHYSICAL LAYOUT (RAW BE16)"],
        &[vec![
            result.device.to_string(),
            result.name,
            result.count.to_string(),
            result.physical_layout_hex,
        ]],
    );
    Ok(())
}

fn gkeys_software_mode(
    discovery: &Discovery,
    index: Option<usize>,
    enabled: bool,
    json: bool,
) -> Result<()> {
    let target = single_device(discovery, index)?;
    GKeys::new(&target.device)?.enable_software_control(enabled)?;
    print_feature_write(
        FeatureWriteResult {
            device: target.index,
            name: target.name.clone(),
            feature: "gkeys-software-mode".into(),
            value: if enabled { "on" } else { "off" }.into(),
            status: "set".into(),
        },
        json,
    )
}

fn mkeys_set(
    discovery: &Discovery,
    index: Option<usize>,
    key: MKeySelection,
    json: bool,
) -> Result<()> {
    let target = single_device(discovery, index)?;
    let mkeys = mkeys::MKeys::new(&target.device)?;
    let count = mkeys.get_count()?;
    if !matches!(key, MKeySelection::None) {
        let selected = match key {
            MKeySelection::M1 => 1,
            MKeySelection::M2 => 2,
            MKeySelection::M3 => 3,
            MKeySelection::None => unreachable!(),
        };
        ensure!(selected <= count, "device reports only {count} M-keys");
    }
    mkeys.set_leds(key.mask())?;
    print_feature_write(
        FeatureWriteResult {
            device: target.index,
            name: target.name.clone(),
            feature: "mkey-led".into(),
            value: key.name().into(),
            status: "set".into(),
        },
        json,
    )
}

fn mr_set(discovery: &Discovery, index: Option<usize>, enabled: bool, json: bool) -> Result<()> {
    let target = single_device(discovery, index)?;
    mkeys::MrKey::new(&target.device)?.set_led(enabled)?;
    print_feature_write(
        FeatureWriteResult {
            device: target.index,
            name: target.name.clone(),
            feature: "mr-led".into(),
            value: if enabled { "on" } else { "off" }.into(),
            status: "set".into(),
        },
        json,
    )
}

fn keys_list(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let target = single_device(discovery, index)?;
    let keys = SpecialKeys::new(&target.device)?;
    // getCapabilities (fn 4) only exists on newer 0x1B04 versions; older boards answer error 7.
    let capabilities = keys.capabilities().unwrap_or(0);
    let rows = keys
        .all_cid_info()?
        .into_iter()
        .map(|info| {
            let (reporting, reporting_error) = match keys.reporting(info.cid) {
                Ok(reporting) => (Some(reporting), None),
                Err(error) => (None, Some(error.to_string())),
            };
            let mut flags = info
                .flags
                .names()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            flags.extend(info.additional_flags.names().into_iter().map(str::to_owned));
            SpecialKeyRow {
                device: target.index,
                index: info.index,
                cid: info.cid,
                cid_hex: format!("0x{:04X}", info.cid),
                name: info.name,
                task_id: info.task_id,
                task_hex: format!("0x{:04X}", info.task_id),
                flags_raw: info.flags.raw,
                additional_flags_raw: info.additional_flags.raw,
                flags,
                position: info.position,
                group: info.group,
                group_mask: info.group_mask,
                reporting,
                reporting_error,
            }
        })
        .collect::<Vec<_>>();
    let result = SpecialKeysListResult {
        device: target.index,
        name: target.name.clone(),
        capabilities,
        keys: rows,
    };
    if json {
        return print_json(&result);
    }
    println!(
        "device {} {} capabilities=0x{:02X}",
        result.device, result.name, result.capabilities
    );
    let rows = result
        .keys
        .iter()
        .map(|row| {
            vec![
                row.index.to_string(),
                row.cid_hex.clone(),
                row.name.clone(),
                row.task_hex.clone(),
                if row.flags.is_empty() {
                    "-".into()
                } else {
                    row.flags.join(",")
                },
                row.reporting
                    .map(|reporting| reporting.divert.to_string())
                    .unwrap_or_else(|| "?".into()),
                row.reporting_error.clone().unwrap_or_default(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &["INDEX", "CID", "NAME", "TASK", "FLAGS", "DIVERT", "ERROR"],
        &rows,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn keys_divert(
    discovery: &Discovery,
    index: Option<usize>,
    cid_value: &str,
    enabled: bool,
    persist: bool,
    raw_xy: Option<bool>,
    json: bool,
) -> Result<()> {
    let target = single_device(discovery, index)?;
    let keys = SpecialKeys::new(&target.device)?;
    let cid = resolve_cid(cid_value)?;
    let info = keys
        .all_cid_info()?
        .into_iter()
        .find(|info| info.cid == cid)
        .with_context(|| format!("CID 0x{cid:04X} is not present on this device"))?;
    ensure!(
        info.flags.divertable,
        "CID 0x{cid:04X} ({}) is not divertable",
        info.name
    );
    if persist {
        ensure!(
            info.flags.persistently_divertable,
            "CID 0x{cid:04X} ({}) is not persistently divertable",
            info.name
        );
    }
    if raw_xy == Some(true) {
        ensure!(
            info.additional_flags.raw_xy,
            "CID 0x{cid:04X} ({}) has no raw-XY capability",
            info.name
        );
    }
    let reporting = keys.update_reporting(
        cid,
        ReportingUpdate {
            divert: Some(enabled),
            persist: persist.then_some(enabled),
            raw_xy,
            ..Default::default()
        },
    )?;
    print_special_write(
        SpecialKeyWriteResult {
            device: target.index,
            name: target.name.clone(),
            cid: Some(cid),
            operation: format!(
                "divert {}{}{}",
                if enabled { "on" } else { "off" },
                if persist { ", persist updated" } else { "" },
                raw_xy
                    .map(|value| if value { ", raw-xy on" } else { ", raw-xy off" })
                    .unwrap_or("")
            ),
            reporting: Some(reporting),
            status: "set and read back".into(),
        },
        json,
    )
}

fn keys_remap(
    discovery: &Discovery,
    index: Option<usize>,
    cid: u16,
    target_cid: u16,
    json: bool,
) -> Result<()> {
    let target = single_device(discovery, index)?;
    let keys = SpecialKeys::new(&target.device)?;
    let infos = keys.all_cid_info()?;
    let source = infos
        .iter()
        .find(|info| info.cid == cid)
        .with_context(|| format!("source CID 0x{cid:04X} is not present"))?;
    let destination = infos
        .iter()
        .find(|info| info.cid == target_cid)
        .with_context(|| format!("target CID 0x{target_cid:04X} is not present"))?;
    ensure_can_remap(source, destination)?;
    let reporting = keys.update_reporting(
        cid,
        ReportingUpdate {
            remap: Some(target_cid),
            ..Default::default()
        },
    )?;
    print_special_write(
        SpecialKeyWriteResult {
            device: target.index,
            name: target.name.clone(),
            cid: Some(cid),
            operation: format!("remap 0x{cid:04X} to 0x{target_cid:04X}"),
            reporting: Some(reporting),
            status: "set and read back".into(),
        },
        json,
    )
}

fn keys_reset(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let target = single_device(discovery, index)?;
    SpecialKeys::new(&target.device)?.reset_all()?;
    print_special_write(
        SpecialKeyWriteResult {
            device: target.index,
            name: target.name.clone(),
            cid: None,
            operation: "reset all CID reporting settings".into(),
            reporting: None,
            status: "reset".into(),
        },
        json,
    )
}

fn print_feature_write(result: FeatureWriteResult, json: bool) -> Result<()> {
    if json {
        print_json(&result)
    } else {
        print_table(
            &["DEVICE", "NAME", "FEATURE", "VALUE", "STATUS"],
            &[vec![
                result.device.to_string(),
                result.name,
                result.feature,
                result.value,
                result.status,
            ]],
        );
        Ok(())
    }
}

fn print_special_write(result: SpecialKeyWriteResult, json: bool) -> Result<()> {
    if json {
        print_json(&result)
    } else {
        print_table(
            &["DEVICE", "NAME", "CID", "OPERATION", "STATUS"],
            &[vec![
                result.device.to_string(),
                result.name,
                result
                    .cid
                    .map(|cid| format!("0x{cid:04X}"))
                    .unwrap_or_else(|| "all".into()),
                result.operation,
                result.status,
            ]],
        );
        Ok(())
    }
}

fn brightness_get(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let target = single_device(discovery, index)?;
    let brightness = Brightness::new(&target.device)?;
    let info = brightness.info()?;
    let raw = brightness.brightness()?;
    let result = BrightnessResult {
        device: target.index,
        name: target.name.clone(),
        info,
        raw,
        percent: brightness_percent(info, raw),
        // fn 3/4 exist only when capabilities bit2 (on/off) is set; else the device answers error 7.
        illumination: if info.capabilities & 0x04 != 0 {
            Some(brightness.illumination()?)
        } else {
            None
        },
    };
    if json {
        return print_json(&result);
    }
    print_table(
        &[
            "DEVICE",
            "NAME",
            "BRIGHTNESS",
            "RAW",
            "MIN",
            "MAX",
            "STEPS",
            "CAPS",
            "ON",
        ],
        &[vec![
            result.device.to_string(),
            result.name,
            format!("{}%", result.percent),
            result.raw.to_string(),
            result.info.min.to_string(),
            result.info.max.to_string(),
            format!("{} (safe {})", result.info.steps, result.info.safe_steps),
            format!("0x{:02X}", result.info.capabilities),
            result
                .illumination
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
        ]],
    );
    Ok(())
}

fn brightness_set(
    discovery: &Discovery,
    index: Option<usize>,
    value: &str,
    raw_value: Option<u16>,
    json: bool,
) -> Result<()> {
    let target = single_device(discovery, index)?;
    let brightness = Brightness::new(&target.device)?;
    let info = brightness.info()?;
    let requested_raw = if value.eq_ignore_ascii_case("raw") {
        raw_value.context("brightness set raw requires a raw level")?
    } else {
        ensure!(
            raw_value.is_none(),
            "unexpected second value; use `brightness set raw N`"
        );
        let percent = value
            .parse::<u8>()
            .with_context(|| format!("invalid brightness percentage {value:?}"))?;
        brightness_raw(info, percent)?
    };
    ensure!(
        (info.min..=info.max).contains(&requested_raw),
        "raw brightness must be in the device range {}..={} ",
        info.min,
        info.max
    );
    let effective_raw = brightness.set_brightness(requested_raw)?;
    let result = BrightnessSetResult {
        device: target.index,
        name: target.name.clone(),
        requested_raw,
        effective_raw,
        percent: brightness_percent(info, effective_raw),
        status: "set and read back".into(),
    };
    if json {
        print_json(&result)
    } else {
        print_table(
            &[
                "DEVICE",
                "NAME",
                "REQUESTED RAW",
                "EFFECTIVE RAW",
                "PERCENT",
                "STATUS",
            ],
            &[vec![
                result.device.to_string(),
                result.name,
                result.requested_raw.to_string(),
                result.effective_raw.to_string(),
                format!("{}%", result.percent),
                result.status,
            ]],
        );
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn rgb_set(
    discovery: &Discovery,
    index: Option<usize>,
    zone: &str,
    effect_name: &str,
    color: Option<&str>,
    color2: Option<&str>,
    speed: Option<u16>,
    period: Option<u16>,
    brightness: Option<u8>,
    intensity: Option<u8>,
    direction: Option<&str>,
    persistence: Persistence,
    json: bool,
) -> Result<()> {
    let target = single_device(discovery, index)?;
    let effect = Effect::parse(effect_name)?;
    let options = EffectOptions {
        color: color.map(str::parse).transpose()?,
        color2: color2.map(str::parse).transpose()?,
        speed,
        period_ms: period,
        brightness,
        intensity,
        direction: direction.map(parse_direction).transpose()?,
    };
    let zones = apply_rgb_effect(target, zone, effect, &options, persistence)?;
    let result = RgbWriteResult {
        device: target.index,
        name: target.name.clone(),
        effect: effect.name.into(),
        zones,
        persistence: persistence_name(persistence).into(),
        status: "set".into(),
    };
    print_rgb_write(result, json)
}

fn apply_rgb_effect(
    target: &ManagedDevice,
    zone: &str,
    effect: Effect,
    options: &EffectOptions,
    persistence: Persistence,
) -> Result<Vec<u8>> {
    let rgb = RgbEffects::new(&target.device)?;
    let capabilities = rgb.capabilities()?;
    let zones = select_rgb_zones(&capabilities, zone)?;
    let params = encode_effect(effect, options)?;
    for cluster_index in &zones {
        let cluster = &capabilities.clusters[usize::from(*cluster_index)];
        let supported = cluster
            .effects
            .iter()
            .find(|candidate| candidate.id == effect.raw_id)
            .with_context(|| {
                format!(
                    "zone {} does not support effect {} (supported: {})",
                    cluster.index,
                    effect.name,
                    cluster
                        .effects
                        .iter()
                        .map(|effect| effect.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        rgb.set_sw_control(true, true)?;
        rgb.set_effect(cluster.index, supported.index, params, persistence)?;
    }
    Ok(zones)
}

fn rgb_info(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let target = single_device(discovery, index)?;
    let capabilities = RgbEffects::new(&target.device)?.capabilities()?;
    let result = RgbInfoResult {
        device: target.index,
        name: target.name.clone(),
        capabilities,
    };
    if json {
        return print_json(&result);
    }
    print_table(
        &[
            "DEVICE",
            "NAME",
            "CLUSTERS",
            "NV CAPS",
            "EXT CAPS",
            "EXTRA",
            "SW CONTROL",
            "NON-RGB",
            "POWER MODE",
            "POWER CONFIG",
        ],
        &[vec![
            result.device.to_string(),
            result.name,
            result.capabilities.device.cluster_count.to_string(),
            format!("0x{:04X}", result.capabilities.device.nv_caps),
            format!("0x{:04X}", result.capabilities.device.ext_caps),
            format!("0x{:02X}", result.capabilities.device.extra),
            result.capabilities.sw_control.enabled.to_string(),
            result.capabilities.sw_control.non_rgb.to_string(),
            result.capabilities.power_mode.to_string(),
            format!(
                "{}/{}/{}",
                result.capabilities.power_mode_config.value_1,
                result.capabilities.power_mode_config.value_2,
                result.capabilities.power_mode_config.value_3
            ),
        ]],
    );
    let rows = result
        .capabilities
        .clusters
        .iter()
        .flat_map(|cluster| {
            cluster.effects.iter().map(move |effect| {
                vec![
                    cluster.index.to_string(),
                    format!("0x{:04X}", cluster.location),
                    format!("0x{:02X}", cluster.persistence_caps),
                    effect.index.to_string(),
                    format!("0x{:04X}", effect.id),
                    effect.name.clone(),
                    format!("0x{:04X}", effect.capabilities),
                    effect.period_ms.to_string(),
                ]
            })
        })
        .collect::<Vec<_>>();
    print_table(
        &[
            "ZONE",
            "LOCATION",
            "PERSIST",
            "INDEX",
            "EFFECT ID",
            "EFFECT",
            "CAPS",
            "PERIOD",
        ],
        &rows,
    );
    Ok(())
}

fn rgb_power(
    discovery: &Discovery,
    index: Option<usize>,
    mode: Option<u8>,
    json: bool,
) -> Result<()> {
    let target = single_device(discovery, index)?;
    let rgb = RgbEffects::new(&target.device)?;
    let status = if let Some(mode) = mode {
        rgb.set_power_mode(mode)?;
        "set and read back"
    } else {
        "read"
    };
    let result = RgbPowerResult {
        device: target.index,
        name: target.name.clone(),
        mode: rgb.power_mode()?,
        status: status.into(),
    };
    if json {
        print_json(&result)
    } else {
        print_table(
            &["DEVICE", "NAME", "POWER MODE", "STATUS"],
            &[vec![
                result.device.to_string(),
                result.name,
                result.mode.to_string(),
                result.status,
            ]],
        );
        Ok(())
    }
}

fn rgb_nv_get(discovery: &Discovery, index: Option<usize>, item: u16, json: bool) -> Result<()> {
    let target = single_device(discovery, index)?;
    let rgb = RgbEffects::new(&target.device)?;
    let info = rgb.device_info()?;
    ensure!(
        info.nv_caps & item != 0,
        "NV item 0x{item:04X} is not advertised by device caps 0x{:04X}",
        info.nv_caps
    );
    let value = rgb.get_nv_config(item)?;
    print_rgb_nv(
        RgbNvResult {
            device: target.index,
            name: target.name.clone(),
            item,
            value: hex_bytes(&value),
            status: "read".into(),
        },
        json,
    )
}

fn rgb_nv_set(
    discovery: &Discovery,
    index: Option<usize>,
    item: u16,
    values: &[String],
    json: bool,
) -> Result<()> {
    let target = single_device(discovery, index)?;
    let rgb = RgbEffects::new(&target.device)?;
    let info = rgb.device_info()?;
    ensure!(
        info.nv_caps & item != 0,
        "NV item 0x{item:04X} is not advertised by device caps 0x{:04X}",
        info.nv_caps
    );
    let value = parse_seven_hex_bytes(values)?;
    rgb.set_nv_config(item, value)?;
    print_rgb_nv(
        RgbNvResult {
            device: target.index,
            name: target.name.clone(),
            item,
            value: hex_bytes(&value),
            status: "set".into(),
        },
        json,
    )
}

fn perkey_set(
    discovery: &Discovery,
    index: Option<usize>,
    scheme: Option<ZoneScheme>,
    assignments: &[String],
    persistent: bool,
    json: bool,
) -> Result<()> {
    let target = single_device(discovery, index)?;
    let scheme = perkey_scheme(target, scheme)?;
    let mut keys = Vec::with_capacity(assignments.len());
    let mut colors = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let (key, color) = assignment
            .split_once('=')
            .with_context(|| format!("assignment {assignment:?} must be <key>=RRGGBB"))?;
        let resolved = resolve_key(key, scheme)?;
        validate_model_key(target, &resolved)?;
        colors.push((resolved.zone_id, color.parse::<RgbColor>()?));
        keys.push(resolved);
    }
    prepare_perkey(target)?;
    let requests = PerKeyLightingV2::new(&target.device)?.write_colors(&colors, persistent)?;
    print_perkey_result(
        target,
        scheme,
        keys,
        colors.len(),
        requests,
        persistent,
        json,
    )
}

fn perkey_fill(
    discovery: &Discovery,
    index: Option<usize>,
    scheme: Option<ZoneScheme>,
    color: &str,
    persistent: bool,
    json: bool,
) -> Result<()> {
    let target = single_device(discovery, index)?;
    let scheme = perkey_scheme(target, scheme)?;
    let map = target
        .model
        .and_then(|model| model.per_key_map.as_ref())
        .context(
            "this device has no embedded per-key usage map; use `perkey set` with explicit keys",
        )?;
    let usages = map
        .entries
        .keys()
        .map(|value| {
            value
                .parse::<u8>()
                .with_context(|| format!("invalid HID usage {value:?} in device registry"))
        })
        .collect::<Result<Vec<_>>>()?;
    let zones = zones_from_usages(usages, scheme)?;
    let color = color.parse::<RgbColor>()?;
    let colors = zones
        .iter()
        .copied()
        .map(|zone| (zone, color))
        .collect::<Vec<_>>();
    prepare_perkey(target)?;
    let requests = PerKeyLightingV2::new(&target.device)?.write_colors(&colors, persistent)?;
    print_perkey_result(
        target,
        scheme,
        Vec::new(),
        colors.len(),
        requests,
        persistent,
        json,
    )
}

fn perkey_frame(
    discovery: &Discovery,
    index: Option<usize>,
    scheme: Option<ZoneScheme>,
    path: &std::path::Path,
    persistent: bool,
    json: bool,
) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let frame: std::collections::BTreeMap<String, String> = serde_json::from_slice(&bytes)
        .with_context(|| {
            format!(
                "failed to parse {} as a key-to-color JSON object",
                path.display()
            )
        })?;
    let assignments = frame
        .into_iter()
        .map(|(key, color)| format!("{key}={color}"))
        .collect::<Vec<_>>();
    perkey_set(discovery, index, scheme, &assignments, persistent, json)
}

fn perkey_probe(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let target = single_device(discovery, index)?;
    prepare_perkey(target)?;
    let perkey = PerKeyLightingV2::new(&target.device)?;
    let probe = probe_zones();
    perkey.set_individual(&probe)?;
    perkey.frame_end(false, 0, 0)?;

    let hidusage = "A=red, B=green (D=red/E=green means Solaar instead)";
    let solaar = "A=blue, B=yellow (D=red/E=green)";
    eprintln!("HID-usage hypothesis: {hidusage}");
    eprintln!("Solaar hypothesis: {solaar}");
    eprint!("Type which keys/colors lit (observation only; no automatic decision): ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;

    perkey.set_single_value(RgbColor::BLACK, &[1, 2, 4, 5])?;
    perkey.frame_end(false, 0, 0)?;
    let result = PerKeyProbeResult {
        device: target.index,
        name: target.name.clone(),
        hidusage_expected: hidusage.into(),
        solaar_expected: solaar.into(),
        answer: answer.trim().into(),
        status: "observation recorded; probe colors cleared; scheme not selected automatically"
            .into(),
    };
    if json {
        print_json(&result)
    } else {
        print_table(
            &["DEVICE", "NAME", "OBSERVATION", "STATUS"],
            &[vec![
                result.device.to_string(),
                result.name,
                result.answer,
                result.status,
            ]],
        );
        Ok(())
    }
}

fn prepare_perkey(target: &ManagedDevice) -> Result<()> {
    let rgb = RgbEffects::new(&target.device)
        .context("per-key lighting requires the device's 0x8071 RGB control feature")?;
    let capabilities = rgb.capabilities()?;
    let cluster = capabilities
        .clusters
        .iter()
        .filter(|cluster| cluster.effects.iter().any(|effect| effect.id == 0x01))
        .find(|cluster| cluster.location & 0x0002 != 0)
        .or_else(|| {
            capabilities
                .clusters
                .iter()
                .find(|cluster| cluster.effects.iter().any(|effect| effect.id == 0x01))
        })
        .context("no RGB cluster supports the fixed effect required for per-key frames")?;
    let fixed = cluster
        .effects
        .iter()
        .find(|effect| effect.id == 0x01)
        .context("selected RGB cluster lost its fixed-effect capability")?;
    let params = encode_effect(
        Effect::parse("fixed")?,
        &EffectOptions {
            color: Some(RgbColor::BLACK),
            ..Default::default()
        },
    )?;
    rgb.set_sw_control(true, true)?;
    if let Err(error) = rgb.set_effect(cluster.index, fixed.index, params, Persistence::Ram) {
        let _ = rgb.set_sw_control(false, false);
        return Err(error);
    }
    Ok(())
}

fn perkey_scheme(target: &ManagedDevice, explicit: Option<ZoneScheme>) -> Result<ZoneScheme> {
    if let Some(scheme) = explicit {
        return Ok(scheme);
    }
    target
        .model
        .and_then(|model| model.per_key_map.as_ref())
        .and_then(|map| map.zone_scheme.as_deref())
        .map(str::parse)
        .transpose()?
        .context("per-key zone numbering is unresolved for this device; pass --zone-scheme hidusage|solaar or declare zone_scheme in data/devices.json")
}

fn validate_model_key(target: &ManagedDevice, key: &ResolvedKey) -> Result<()> {
    let Some(usage) = key.usage else {
        return Ok(());
    };
    let Some(map) = target.model.and_then(|model| model.per_key_map.as_ref()) else {
        return Ok(());
    };
    ensure!(
        map.entries.contains_key(&usage.to_string()),
        "key {} (HID usage 0x{usage:02X}) is not present in this device's per-key map",
        key.name
    );
    Ok(())
}

fn select_rgb_zones(capabilities: &RgbCapabilities, value: &str) -> Result<Vec<u8>> {
    if value.eq_ignore_ascii_case("all") || value.eq_ignore_ascii_case("ZONE_ALL") {
        return Ok(capabilities
            .clusters
            .iter()
            .map(|cluster| cluster.index)
            .collect());
    }
    if let Some(location) = ghub_zone_location(value) {
        let zones = capabilities
            .clusters
            .iter()
            .filter(|cluster| cluster.location == location)
            .map(|cluster| cluster.index)
            .collect::<Vec<_>>();
        ensure!(
            !zones.is_empty(),
            "RGB zone {value} (location 0x{location:04X}) is not reported by this device"
        );
        return Ok(zones);
    }
    let zone = parse_u8_arg(value).map_err(anyhow::Error::msg)?;
    ensure!(
        capabilities
            .clusters
            .iter()
            .any(|cluster| cluster.index == zone),
        "RGB zone {zone} does not exist (valid range: 0..{})",
        capabilities.device.cluster_count.saturating_sub(1)
    );
    Ok(vec![zone])
}

fn ghub_zone_location(value: &str) -> Option<u16> {
    Some(match value.to_ascii_uppercase().as_str() {
        "ZONE_PRIMARY" => 2,
        "ZONE_LOGO" | "ZONE_BRANDING" => 4,
        "ZONE_ONE" => 8,
        "ZONE_TWO" => 16,
        "ZONE_THREE" => 32,
        "ZONE_FOUR" => 64,
        "ZONE_FIVE" => 128,
        "ZONE_SIX" => 256,
        "ZONE_SEVEN" => 512,
        "ZONE_LEFT_SIDE" => 1024,
        "ZONE_RIGHT_SIDE" => 2048,
        "ZONE_COMBINED" => 4096,
        "ZONE_TOP" => 8192,
        "ZONE_BOTTOM" => 16384,
        "ZONE_HALO" => 32768,
        "ZONE_IDLE_STATE" => 129,
        "ZONE_IN_USE_STATE" => 130,
        "ZONE_MUTED_STATE" => 131,
        "ZONE_SOFT_MUTED_STATE" => 132,
        "ZONE_FULL_SUPPORT" => 65535,
        _ => return None,
    })
}

fn print_rgb_write(result: RgbWriteResult, json: bool) -> Result<()> {
    if json {
        print_json(&result)
    } else {
        print_table(
            &["DEVICE", "NAME", "ZONES", "EFFECT", "PERSISTENCE", "STATUS"],
            &[vec![
                result.device.to_string(),
                result.name,
                result
                    .zones
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                result.effect,
                result.persistence,
                result.status,
            ]],
        );
        Ok(())
    }
}

fn print_rgb_nv(result: RgbNvResult, json: bool) -> Result<()> {
    if json {
        print_json(&result)
    } else {
        print_table(
            &["DEVICE", "NAME", "ITEM", "VALUE", "STATUS"],
            &[vec![
                result.device.to_string(),
                result.name,
                format!("0x{:04X}", result.item),
                result.value,
                result.status,
            ]],
        );
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn print_perkey_result(
    target: &ManagedDevice,
    scheme: ZoneScheme,
    keys: Vec<ResolvedKey>,
    zone_count: usize,
    requests: usize,
    persistent: bool,
    json: bool,
) -> Result<()> {
    let result = PerKeyWriteResult {
        device: target.index,
        name: target.name.clone(),
        zone_scheme: scheme,
        keys,
        zone_count,
        requests,
        persistent,
        status: "frame committed".into(),
    };
    if json {
        print_json(&result)
    } else {
        print_table(
            &[
                "DEVICE",
                "NAME",
                "ZONE SCHEME",
                "ZONES",
                "REQUESTS",
                "PERSISTENT",
                "STATUS",
            ],
            &[vec![
                result.device.to_string(),
                result.name,
                result.zone_scheme.to_string(),
                result.zone_count.to_string(),
                result.requests.to_string(),
                result.persistent.to_string(),
                result.status,
            ]],
        );
        Ok(())
    }
}

fn persistence_name(value: Persistence) -> &'static str {
    match value {
        Persistence::Ram => "ram",
        Persistence::Nvm => "nvm",
        Persistence::PowerSave => "powersave",
    }
}

fn parse_seven_hex_bytes(values: &[String]) -> Result<[u8; 7]> {
    let parts = if values.len() == 1 && values[0].len() == 14 {
        values[0]
            .as_bytes()
            .chunks_exact(2)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect::<Vec<_>>()
    } else {
        values.to_vec()
    };
    ensure!(
        parts.len() == 7,
        "NV value must contain exactly 7 hex bytes"
    );
    let mut result = [0_u8; 7];
    for (index, value) in parts.iter().enumerate() {
        let value = value.strip_prefix("0x").unwrap_or(value);
        ensure!(
            value.len() == 2,
            "NV byte {value:?} must contain two hex digits"
        );
        result[index] =
            u8::from_str_radix(value, 16).with_context(|| format!("invalid NV byte {value:?}"))?;
    }
    Ok(result)
}

fn profile_import_ghub(
    db: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    device_model: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let db_path = match db {
        Some(path) => path,
        None => default_ghub_db_path()?,
    };
    let output_dir = match out_dir {
        Some(path) => path,
        None => default_output_dir()?,
    };
    let mut imported = import_ghub_database(&db_path, device_model)?;
    let paths = output_paths(&imported, &output_dir);
    imported.summary.output_files = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    imported.summary.dry_run = dry_run;
    if !dry_run {
        save_import(&imported, &output_dir)?;
    }
    print_import_summary(&imported, json)
}

fn print_import_summary(imported: &ImportResult, json: bool) -> Result<()> {
    if json {
        return print_json(&imported.summary);
    }
    let summary = &imported.summary;
    println!("G HUB import: {}", summary.database);
    println!(
        "profiles: {} found / {} imported; cards: {}; applications: {}; assignments: {}",
        summary.profiles_found,
        summary.profiles_imported,
        summary.cards,
        summary.applications,
        summary.assignments
    );
    println!(
        "macro cards: {}; lighting cards: {}; device models: {}",
        summary.macro_cards,
        summary.lighting_cards,
        if summary.device_models.is_empty() {
            "-".into()
        } else {
            summary.device_models.join(", ")
        }
    );
    println!(
        "{}: {}",
        if summary.dry_run {
            "would write"
        } else {
            "wrote"
        },
        if summary.output_files.is_empty() {
            "nothing".into()
        } else {
            summary.output_files.join(", ")
        }
    );
    if summary.dry_run {
        println!("dry-run: no files written");
    }
    if summary.unmapped_classes.is_empty() {
        println!("unmapped: none");
    } else {
        println!("unmapped classes:");
        for (class, count) in &summary.unmapped_classes {
            println!("  {class}: {count}");
        }
    }
    if summary.warnings.is_empty() {
        println!("warnings: none");
    } else {
        println!("warnings ({}):", summary.warnings.len());
        for warning in &summary.warnings {
            let location = match (&warning.profile, &warning.slot_id) {
                (Some(profile), Some(slot)) => format!(" [{profile} / {slot}]"),
                (Some(profile), None) => format!(" [{profile}]"),
                (None, Some(slot)) => format!(" [{slot}]"),
                (None, None) => String::new(),
            };
            println!("  {}{}: {}", warning.class, location, warning.reason);
        }
    }
    Ok(())
}

fn profile_list(json: bool) -> Result<()> {
    let store = load_store(&default_store_path()?)?;
    print_profile_rows(&store.profiles, json)
}

fn profile_show(name: Option<&str>, json: bool) -> Result<()> {
    let store = load_store(&default_store_path()?)?;
    let profiles = store
        .profiles
        .iter()
        .filter(|profile| name.is_none_or(|name| profile.name == name))
        .collect::<Vec<_>>();
    if let Some(name) = name {
        ensure!(!profiles.is_empty(), "profile {name:?} was not found");
    }
    if json {
        return print_json(&profiles);
    }
    if profiles.is_empty() {
        println!("No saved profiles.");
        return Ok(());
    }
    for profile in profiles {
        println!(
            "{} ({}) — models: {}; DPI: {} [{}]; shift: {}; rate: {} Hz",
            profile.name,
            profile.source,
            if profile.device_models.is_empty() {
                "-".into()
            } else {
                profile.device_models.join(", ")
            },
            profile.active_dpi,
            profile
                .dpi_levels
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            profile.shift_dpi,
            profile.report_rate_hz
        );
        if !profile.bindings.is_empty() {
            let rows = profile
                .bindings
                .iter()
                .map(|binding| {
                    vec![
                        binding.slot_id.clone(),
                        binding.source_action.clone(),
                        binding.onboard_binding.clone(),
                        binding
                            .daemon_action
                            .as_ref()
                            .map(|action| serde_json::to_string(action).unwrap_or_default())
                            .unwrap_or_else(|| "-".into()),
                    ]
                })
                .collect::<Vec<_>>();
            print_table(&["SLOT", "SOURCE", "ONBOARD", "DAEMON"], &rows);
        }
        if !profile.macros.is_empty() {
            let rows = profile
                .macros
                .iter()
                .map(|r#macro| {
                    vec![
                        r#macro.name.clone(),
                        r#macro.macro_type.clone(),
                        r#macro.daemon_action.is_some().to_string(),
                        r#macro.onboard_macro.is_some().to_string(),
                    ]
                })
                .collect::<Vec<_>>();
            print_table(&["MACRO", "TYPE", "DAEMON", "ONBOARD"], &rows);
        }
        if !profile.lighting.is_empty() {
            let rows = profile
                .lighting
                .iter()
                .map(|preset| {
                    vec![
                        preset.zone.clone(),
                        preset.effect.clone(),
                        preset.color.clone().unwrap_or_else(|| "-".into()),
                        preset.persist.clone(),
                    ]
                })
                .collect::<Vec<_>>();
            print_table(&["ZONE", "EFFECT", "COLOR", "PERSIST"], &rows);
        }
        for warning in &profile.warnings {
            println!("warning: {warning}");
        }
    }
    Ok(())
}

fn profile_apply(
    name: &str,
    index: Option<usize>,
    onboard_mode: bool,
    yes: bool,
    json: bool,
) -> Result<()> {
    let store = load_store(&default_store_path()?)?;
    let profile = store
        .profiles
        .iter()
        .find(|profile| profile.name == name)
        .with_context(|| format!("profile {name:?} was not found"))?;
    if onboard_mode {
        ensure!(
            yes,
            "onboard profile apply writes device memory and requires --yes"
        );
        return profile_apply_onboard(profile, index, json);
    }
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    ensure_profile_matches_device(profile, target)?;

    if profile.active_dpi != 0 {
        target
            .device
            .set_dpi(profile.active_dpi)
            .map_err(anyhow::Error::new)
            .with_context(|| format!("failed to apply {} DPI", profile.active_dpi))?;
    }
    if profile.report_rate_hz != 0 {
        target
            .device
            .set_report_rate(profile.report_rate_hz)
            .map_err(anyhow::Error::new)
            .with_context(|| {
                format!("failed to apply {} Hz report rate", profile.report_rate_hz)
            })?;
    }
    for preset in &profile.lighting {
        apply_profile_rgb(target, preset)?;
    }

    let active_dpi = if profile.active_dpi == 0 {
        0
    } else {
        target
            .device
            .dpi()
            .map_err(anyhow::Error::new)
            .context("DPI was set but could not be read back")?
    };
    let report_rate_hz = if profile.report_rate_hz == 0 {
        0
    } else {
        target
            .device
            .report_rate()
            .map_err(anyhow::Error::new)
            .context("report rate was set but could not be read back")?
    };
    let result = ProfileApplyResult {
        profile: profile.name.clone(),
        device: target.index,
        device_name: target.name.clone(),
        active_dpi,
        report_rate_hz,
        lighting_effects: profile.lighting.len(),
        mode: "live".into(),
    };

    if json {
        print_json(&result)
    } else {
        print_table(
            &[
                "PROFILE",
                "DEVICE",
                "NAME",
                "ACTIVE DPI",
                "RATE (HZ)",
                "LIGHTING",
                "MODE",
            ],
            &[vec![
                result.profile,
                result.device.to_string(),
                result.device_name,
                result.active_dpi.to_string(),
                result.report_rate_hz.to_string(),
                result.lighting_effects.to_string(),
                result.mode,
            ]],
        );
        Ok(())
    }
}

fn ensure_profile_matches_device(profile: &Profile, target: &ManagedDevice) -> Result<()> {
    if profile.device_models.is_empty() {
        return Ok(());
    }
    let model = target
        .model
        .context("the selected device has no recognized model id")?;
    ensure!(
        profile
            .device_models
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(&model.model_id)),
        "profile {:?} targets {}, but selected device is {}",
        profile.name,
        profile.device_models.join(", "),
        model.model_id
    );
    Ok(())
}

fn apply_profile_rgb(target: &ManagedDevice, preset: &RgbPreset) -> Result<()> {
    let effect = Effect::parse(&preset.effect)?;
    let options = EffectOptions {
        color: preset.color.as_deref().map(str::parse).transpose()?,
        color2: preset.color2.as_deref().map(str::parse).transpose()?,
        speed: preset.speed,
        period_ms: preset.period,
        brightness: preset.brightness,
        intensity: preset.intensity,
        direction: preset
            .direction
            .as_deref()
            .map(parse_direction)
            .transpose()?,
    };
    let persistence = match preset.persist.to_ascii_lowercase().as_str() {
        "ram" => Persistence::Ram,
        "nvm" | "nv" => Persistence::Nvm,
        "powersave" | "power_save" => Persistence::PowerSave,
        value => bail!("unknown RGB persistence {value:?}"),
    };
    apply_rgb_effect(target, &preset.zone, effect, &options, persistence)?;
    Ok(())
}

fn profile_apply_onboard(profile: &Profile, index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    ensure_profile_matches_device(profile, target)?;
    let model = target.model.context("selected device model is unknown")?;
    let onboard = Onboard::new(&target.device)?;
    let current = onboard.dump()?;
    let mut exported = export_state(&current, device_kind(target))?;
    for warning in apply_to_onboard_export(profile, &model.model_id, &mut exported)? {
        eprintln!("warning: {warning}");
    }
    let diffs = import_plan(&exported, &current, device_kind(target))?;
    print_sector_diff(&diffs, false, json)?;
    if diffs.is_empty() {
        return Ok(());
    }
    require_backup(&current.description)?;
    let methods = write_sector_diffs(
        &onboard,
        &current.description,
        &exported.directory.entries,
        &diffs,
    )?;
    print_onboard_result(
        OnboardWriteResult {
            device: target.index,
            name: target.name.clone(),
            operation: format!(
                "apply onboard profile {:?} ({} sectors)",
                profile.name,
                diffs.len()
            ),
            status: verification_summary(&methods),
        },
        json,
    )
}

fn print_profile_rows(profiles: &[Profile], json: bool) -> Result<()> {
    if json {
        return print_json(profiles);
    }
    let rows = profiles
        .iter()
        .map(|profile| {
            vec![
                profile.name.clone(),
                profile.active_dpi.to_string(),
                profile
                    .dpi_levels
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                profile.report_rate_hz.to_string(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["NAME", "ACTIVE DPI", "LEVELS", "RATE (HZ)"], &rows);
    Ok(())
}

fn onboard_info(index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    let mode = mode_name(onboard.mode()?);
    let current_profile = onboard.current_profile()?;
    let current_dpi_index = onboard.current_dpi_index()?;
    let (directory_raw, directory) = onboard.directory(&description)?;
    let result = OnboardInfoResult {
        device: target.index,
        name: target.name.clone(),
        description,
        mode,
        current_profile,
        current_dpi_index,
        directory,
        directory_raw: hex_bytes(&directory_raw),
    };
    if json {
        return print_json(&result);
    }
    print_table(
        &[
            "DEVICE",
            "NAME",
            "MODE",
            "MEMORY",
            "PROFILE",
            "MACRO",
            "SECTOR SIZE",
            "ACTIVE PROFILE",
            "DPI SLOT",
        ],
        &[vec![
            result.device.to_string(),
            result.name,
            result.mode,
            format!("0x{:02X}", result.description.memory_model_id),
            format!("0x{:02X}", result.description.profile_format_id),
            format!("0x{:02X}", result.description.macro_format_id),
            result.description.sector_size.to_string(),
            format!("0x{:04X}", result.current_profile),
            result.current_dpi_index.to_string(),
        ]],
    );
    println!("DESCRIPTION RAW: {}", hex_bytes(&result.description.raw));
    let rows = result
        .directory
        .iter()
        .map(|entry| {
            vec![
                entry.index.to_string(),
                format!("0x{:04X}", entry.sector),
                entry.enabled.to_string(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["PROFILE", "SECTOR", "ENABLED"], &rows);
    println!("DIRECTORY RAW: {}", result.directory_raw);
    Ok(())
}

fn onboard_dump(out: &std::path::Path, index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let dump = onboard.dump()?;
    let sector_count = dump.sectors.len();
    let bytes = encode_dump(&dump)?;
    save_dump(out, &bytes)?;
    let safety_path = backup_path()?;
    if safety_path != out {
        save_dump(&safety_path, &bytes)?;
    }
    let result = OnboardWriteResult {
        device: target.index,
        name: target.name.clone(),
        operation: format!("dump {sector_count} sectors to {}", out.display()),
        status: "saved and registered as safety backup".into(),
    };
    print_onboard_result(result, json)?;
    if !json {
        let export = export_state(&dump, device_kind(target))?;
        let rows = export
            .profiles
            .iter()
            .flat_map(macro_rows)
            .collect::<Vec<_>>();
        if !rows.is_empty() {
            println!("decoded macro bindings:");
            let table = rows
                .iter()
                .map(|row| {
                    vec![
                        row["profile"].as_str().unwrap().to_owned(),
                        row["control"].as_str().unwrap().to_owned(),
                        row["gshift"].as_bool().unwrap().to_string(),
                        row["sector"].as_str().unwrap().to_owned(),
                        row["offset"].as_str().unwrap().to_owned(),
                        row["steps"].to_string(),
                    ]
                })
                .collect::<Vec<_>>();
            print_table(
                &["PROFILE", "CONTROL", "G-SHIFT", "SECTOR", "OFFSET", "STEPS"],
                &table,
            );
        }
    }
    Ok(())
}

fn onboard_export(out: &std::path::Path, index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let dump = onboard.dump()?;
    let export = export_state(&dump, device_kind(target))?;
    save_export(out, &export)?;
    let result = OnboardWriteResult {
        device: target.index,
        name: target.name.clone(),
        operation: format!(
            "export {} profiles and {} macro sectors to {}",
            export.profiles.len(),
            export.macro_sectors.len(),
            out.display()
        ),
        status: "saved (read-only)".into(),
    };
    print_onboard_result(result, json)
}

fn onboard_import(
    input: &std::path::Path,
    dry_run: bool,
    yes: bool,
    index: Option<usize>,
    json: bool,
) -> Result<()> {
    let portable = load_portable_onboard(input)?;
    let file_export = if portable.is_none() {
        Some(load_export(input)?)
    } else {
        None
    };
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let current = onboard.dump()?;
    let exported = if let Some(portable) = portable {
        let model = target.model.context("selected device model is unknown")?;
        let expected = device_data::lookup_model(&portable.device_model)
            .map(|record| record.model_id.as_str())
            .unwrap_or(&portable.device_model);
        ensure!(
            model.model_id.eq_ignore_ascii_case(expected),
            "portable profile targets {}, but selected device is {}",
            portable.device_model,
            model.model_id
        );
        let mut exported = export_state(&current, device_kind(target))?;
        for warning in apply_to_onboard_export(&portable.profile, &model.model_id, &mut exported)? {
            eprintln!("warning: {warning}");
        }
        exported
    } else {
        file_export.context("non-portable onboard export was not loaded")?
    };
    let diffs = import_plan(&exported, &current, device_kind(target))?;
    print_sector_diff(&diffs, dry_run, json)?;
    if dry_run || diffs.is_empty() {
        return Ok(());
    }
    ensure!(
        yes,
        "{} differing sectors; refusing to write without --yes (use --dry-run to validate only)",
        diffs.len()
    );
    require_backup(&current.description)?;
    let methods = write_sector_diffs(
        &onboard,
        &current.description,
        &exported.directory.entries,
        &diffs,
    )?;
    let status = verification_summary(&methods);
    print_onboard_result(
        OnboardWriteResult {
            device: target.index,
            name: target.name.clone(),
            operation: format!("import {} sectors from {}", diffs.len(), input.display()),
            status,
        },
        json,
    )
}

fn print_sector_diff(diffs: &[SectorDiff], dry_run: bool, json: bool) -> Result<()> {
    if json {
        return print_json(&serde_json::json!({
            "dry_run": dry_run,
            "differing_sector_count": diffs.len(),
            "diffs": diffs.iter().map(|diff| serde_json::json!({
                "sector": diff.sector,
                "current_crc": diff.current_crc,
                "replacement_crc": diff.replacement_crc,
            })).collect::<Vec<_>>(),
        }));
    }
    println!("{} differing sectors", diffs.len());
    if !diffs.is_empty() {
        let rows = diffs
            .iter()
            .map(|diff| {
                vec![
                    format!("0x{:04X}", diff.sector),
                    format!("0x{:04X}", diff.current_crc),
                    format!("0x{:04X}", diff.replacement_crc),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["SECTOR", "CURRENT CRC", "IMPORTED CRC"], &rows);
    }
    if dry_run {
        println!("dry-run: no sectors written");
    }
    Ok(())
}

fn write_sector_diffs(
    onboard: &Onboard<'_>,
    description: &OnboardDescription,
    entries: &[DirectoryEntry],
    diffs: &[SectorDiff],
) -> Result<Vec<VerificationMethod>> {
    let macro_ids = macro_sector_ids(description, entries)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let ordered = diffs
        .iter()
        .filter(|diff| macro_ids.contains(&diff.sector))
        .chain(
            diffs
                .iter()
                .filter(|diff| diff.sector != 0 && !macro_ids.contains(&diff.sector)),
        )
        .chain(diffs.iter().filter(|diff| diff.sector == 0));
    ordered
        .map(|diff| {
            onboard.write_sector_verified(
                diff.sector,
                &diff.current,
                &diff.replacement,
                diff.sector == 0,
            )
        })
        .collect()
}

fn verification_summary(methods: &[VerificationMethod]) -> String {
    let get_crc = methods
        .iter()
        .filter(|method| **method == VerificationMethod::GetCrc)
        .count();
    let read_back = methods.len() - get_crc;
    format!("verified: GetCRC {get_crc}, read-back {read_back}")
}

fn onboard_get_name(index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    let (_, entries) = onboard.directory(&description)?;
    let sector_id = first_enabled_sector(&entries)?;
    let sector = onboard.read_sector(sector_id, description.sector_size)?;
    let result = OnboardNameResult {
        device: target.index,
        name: target.name.clone(),
        sector: sector_id,
        profile_name: profile_name(&sector)?,
    };
    if json {
        return print_json(&result);
    }
    print_table(
        &["DEVICE", "NAME", "SECTOR", "PROFILE NAME"],
        &[vec![
            result.device.to_string(),
            result.name,
            format!("0x{:04X}", result.sector),
            result.profile_name.unwrap_or_else(|| "-".into()),
        ]],
    );
    Ok(())
}

fn onboard_set_name(name: &str, index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    require_backup(&description)?;
    let (_, entries) = onboard.directory(&description)?;
    let sector_id = first_enabled_sector(&entries)?;
    let original = onboard.read_sector(sector_id, description.sector_size)?;
    let mut replacement = original.clone();
    set_profile_name(&mut replacement, name)?;
    onboard.write_sector_verified(sector_id, &original, &replacement, false)?;
    print_onboard_result(
        OnboardWriteResult {
            device: target.index,
            name: target.name.clone(),
            operation: format!("set profile name in sector 0x{sector_id:04X}"),
            status: "verified".into(),
        },
        json,
    )
}

fn onboard_crc(sector: u16, index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let response = onboard.get_crc(sector)?;
    let result = OnboardCrcResult {
        device: target.index,
        name: target.name.clone(),
        sector,
        crc: response.crc,
        raw: response.raw,
    };
    if json {
        return print_json(&result);
    }
    print_table(
        &["DEVICE", "NAME", "SECTOR", "CRC", "RAW"],
        &[vec![
            result.device.to_string(),
            result.name,
            format!("0x{:04X}", result.sector),
            format!("0x{:04X}", result.crc),
            hex_bytes(&result.raw),
        ]],
    );
    Ok(())
}

fn onboard_exec_macro(sector: u16, offset: u16, index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    onboard.execute_macro(sector, offset)?;
    print_onboard_result(
        OnboardWriteResult {
            device: target.index,
            name: target.name.clone(),
            operation: format!("execute macro at 0x{sector:04X}:0x{offset:04X}"),
            status: "executed".into(),
        },
        json,
    )
}

fn onboard_restore(input: &std::path::Path, index: Option<usize>, json: bool) -> Result<()> {
    let dump = load_dump(input)?;
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    anyhow::ensure!(
        description.raw == dump.description.raw,
        "dump description does not match the target device"
    );

    // Restore profile/data sectors first and the directory last.
    let ordered = dump
        .sectors
        .iter()
        .filter(|(sector, _)| *sector != 0)
        .chain(dump.sectors.iter().filter(|(sector, _)| *sector == 0));
    let mut restored = 0;
    for (sector, replacement) in ordered {
        let current = onboard.read_sector(*sector, description.sector_size)?;
        onboard.write_sector_verified(*sector, &current, replacement, true)?;
        restored += 1;
    }
    let result = OnboardWriteResult {
        device: target.index,
        name: target.name.clone(),
        operation: format!("restore {restored} sectors from {}", input.display()),
        status: "verified".into(),
    };
    print_onboard_result(result, json)
}

fn buttons_list(index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    let (_, entries) = onboard.directory(&description)?;
    let sector_id = first_enabled_sector(&entries)?;
    let sector = onboard.read_sector(sector_id, description.sector_size)?;
    let rows = button_rows(
        &sector,
        description.button_count,
        device_kind(target) == "KEYBOARD",
    )?;
    if json {
        return print_json(&rows);
    }
    print_button_rows(&rows);
    Ok(())
}

fn buttons_set(
    number: usize,
    value: &str,
    gshift: bool,
    index: Option<usize>,
    json: bool,
    require_keyboard: bool,
) -> Result<()> {
    let binding = parse_binding(value)?;
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    ensure!(
        !require_keyboard || device_kind(target) == "KEYBOARD",
        "set-gkey requires a device classified as KEYBOARD"
    );
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    require_backup(&description)?;
    let (_, entries) = onboard.directory(&description)?;
    let sector_id = first_enabled_sector(&entries)?;
    let original = onboard.read_sector(sector_id, description.sector_size)?;
    let mut replacement = original.clone();
    set_onboard_button(
        &mut replacement,
        number,
        gshift,
        &binding,
        description.button_count,
    )?;
    onboard.write_sector_verified(sector_id, &original, &replacement, false)?;
    print_onboard_result(
        OnboardWriteResult {
            device: target.index,
            name: target.name.clone(),
            operation: format!(
                "set {}{} to {binding}",
                if gshift { "G-Shift " } else { "" },
                if device_kind(target) == "KEYBOARD" {
                    format!("g{number}")
                } else {
                    format!("button {number}")
                }
            ),
            status: "verified".into(),
        },
        json,
    )
}

fn onboard_macro_list(index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let export = export_state(&onboard.dump()?, device_kind(target))?;
    let rows = export
        .profiles
        .iter()
        .flat_map(macro_rows)
        .collect::<Vec<_>>();
    if json {
        return print_json(&rows);
    }
    let table = rows
        .iter()
        .map(|row| {
            vec![
                row["profile"].as_str().unwrap().to_owned(),
                row["control"].as_str().unwrap().to_owned(),
                row["gshift"].as_bool().unwrap().to_string(),
                row["sector"].as_str().unwrap().to_owned(),
                row["offset"].as_str().unwrap().to_owned(),
                row["steps"].to_string(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &["PROFILE", "CONTROL", "G-SHIFT", "SECTOR", "OFFSET", "STEPS"],
        &table,
    );
    Ok(())
}

fn macro_rows(profile: &onboard::ExportProfile) -> Vec<serde_json::Value> {
    [(false, &profile.bindings), (true, &profile.gshift_bindings)]
        .into_iter()
        .flat_map(|(gshift, bindings)| {
            bindings.iter().filter_map(move |binding| {
                let OnboardBinding::Macro { sector, offset } =
                    parse_binding(&binding.binding).ok()?
                else {
                    return None;
                };
                Some(serde_json::json!({
                    "profile": format!("{}{}", profile.index, profile.name.as_deref().map_or(String::new(), |name| format!(" ({name})"))),
                    "control": binding.control,
                    "number": binding.number,
                    "gshift": gshift,
                    "sector": format!("0x{sector:04X}"),
                    "offset": format!("0x{offset:04X}"),
                    "steps": binding.r#macro.as_ref().map(|value| &value.steps),
                }))
            })
        })
        .collect()
}

fn onboard_macro_show(sector: u16, offset: u16, index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let dump = onboard.dump()?;
    let directory = dump
        .sectors
        .iter()
        .find(|(id, _)| *id == 0)
        .map(|(_, bytes)| bytes)
        .context("directory sector was not read")?;
    let entries = onboard::parse_directory(directory, dump.description.profile_count)?;
    let macro_ids = macro_sector_ids(&dump.description, &entries)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let sectors = dump.sectors.into_iter().collect::<BTreeMap<_, _>>();
    let value = decode_macro(&sectors, &macro_ids, sector, offset)?;
    if json {
        return print_json(&value);
    }
    println!("macro 0x{sector:04X}:0x{offset:04X}");
    println!("{}", serde_json::to_string_pretty(&value.steps)?);
    Ok(())
}

fn onboard_macro_set(
    number: usize,
    gshift: bool,
    steps: &str,
    index: Option<usize>,
    json: bool,
) -> Result<()> {
    let value = Macro::from_steps_json(steps)?;
    edit_onboard_macro(number, gshift, Some(value), index, json)
}

fn onboard_macro_clear(
    number: usize,
    gshift: bool,
    index: Option<usize>,
    json: bool,
) -> Result<()> {
    edit_onboard_macro(number, gshift, None, index, json)
}

fn edit_onboard_macro(
    number: usize,
    gshift: bool,
    value: Option<Macro>,
    index: Option<usize>,
    json: bool,
) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let current = onboard.dump()?;
    require_backup(&current.description)?;
    let mut export = export_state(&current, device_kind(target))?;
    let macro_sector = *macro_sector_ids(&export.description, &export.directory.entries)?
        .first()
        .unwrap();
    ensure!(number > 0, "control number starts at 1");
    let (control, is_set) = {
        let profile = export
            .profiles
            .iter_mut()
            .find(|profile| profile.enabled)
            .context("profile directory has no enabled profile")?;
        let bindings = if gshift {
            &mut profile.gshift_bindings
        } else {
            &mut profile.bindings
        };
        let count = bindings.len();
        let binding = bindings.get_mut(number - 1).with_context(|| {
            format!(
                "{} number must be 1..={count}",
                if device_kind(target) == "KEYBOARD" {
                    "G-key"
                } else {
                    "button"
                }
            )
        })?;
        if let Some(value) = value {
            binding.binding = OnboardBinding::Macro {
                sector: macro_sector,
                offset: 0,
            }
            .to_string();
            binding.r#macro = Some(value);
        } else {
            binding.binding = OnboardBinding::Disabled.to_string();
            binding.raw_hex = "FF000000".into();
            binding.r#macro = None;
        }
        (binding.control.clone(), binding.r#macro.is_some())
    };
    repack_export_macros(&mut export)?;
    let diffs = import_plan(&export, &current, device_kind(target))?;
    let methods = write_sector_diffs(
        &onboard,
        &current.description,
        &export.directory.entries,
        &diffs,
    )?;
    print_onboard_result(
        OnboardWriteResult {
            device: target.index,
            name: target.name.clone(),
            operation: format!(
                "{} {}{}",
                if is_set {
                    "set macro on"
                } else {
                    "clear macro from"
                },
                if gshift { "G-Shift " } else { "" },
                control
            ),
            status: verification_summary(&methods),
        },
        json,
    )
}

fn onboard_led_show(index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    let (_, entries) = onboard.directory(&description)?;
    let sector_id = first_enabled_sector(&entries)?;
    let sector = onboard.read_sector(sector_id, description.sector_size)?;
    let slots = led_slots(&sector)?;
    if json {
        return print_json(&slots);
    }
    let rows = slots
        .iter()
        .map(|slot| {
            vec![
                slot.slot.to_string(),
                slot.effect.clone(),
                format!("0x{:02X}", slot.raw_id),
                slot.parameters_hex.clone(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["SLOT", "EFFECT", "RAW ID", "PARAMETERS"], &rows);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn onboard_led_set(
    slot: usize,
    effect_name: &str,
    color: Option<&str>,
    color2: Option<&str>,
    speed: Option<u16>,
    period: Option<u16>,
    brightness: Option<u8>,
    intensity: Option<u8>,
    direction: Option<&str>,
    index: Option<usize>,
    json: bool,
) -> Result<()> {
    let effect = Effect::parse(effect_name)?;
    let options = EffectOptions {
        color: color.map(str::parse).transpose()?,
        color2: color2.map(str::parse).transpose()?,
        speed,
        period_ms: period,
        brightness,
        intensity,
        direction: direction.map(parse_direction).transpose()?,
    };
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    require_backup(&description)?;
    let (_, entries) = onboard.directory(&description)?;
    let sector_id = first_enabled_sector(&entries)?;
    let original = onboard.read_sector(sector_id, description.sector_size)?;
    let mut replacement = original.clone();
    set_led_slot(&mut replacement, slot, effect, &options)?;
    let method = onboard.write_sector_verified(sector_id, &original, &replacement, false)?;
    print_onboard_result(
        OnboardWriteResult {
            device: target.index,
            name: target.name.clone(),
            operation: format!("set onboard LED slot {slot} to {}", effect.name),
            status: format!("verified by {method}"),
        },
        json,
    )
}

fn onboard_set_dpi(
    levels: &[u16],
    default: usize,
    shift: Option<u16>,
    index: Option<usize>,
    json: bool,
) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    require_backup(&description)?;
    let (_, entries) = onboard.directory(&description)?;
    let sector_id = first_enabled_sector(&entries)?;
    let original = onboard.read_sector(sector_id, description.sector_size)?;
    let mut replacement = original.clone();
    set_onboard_dpi(&mut replacement, levels, default, shift)?;
    onboard.write_sector_verified(sector_id, &original, &replacement, false)?;
    // Profile rewrites reset the live DPI slot to 0; move it to the new default.
    onboard.set_current_dpi_index(default as u8)?;
    print_onboard_result(
        OnboardWriteResult {
            device: target.index,
            name: target.name.clone(),
            operation: format!(
                "set DPI levels [{}], default index {default}",
                levels
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            status: "verified".into(),
        },
        json,
    )
}

fn onboard_set_dpi_index(dpi_index: u8, index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    onboard.set_current_dpi_index(dpi_index)?;
    print_onboard_result(
        OnboardWriteResult {
            device: target.index,
            name: target.name.clone(),
            operation: format!("set current onboard DPI slot to {dpi_index}"),
            status: "verified".into(),
        },
        json,
    )
}

fn onboard_set_rate(hz: u32, index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    require_backup(&description)?;
    let (_, entries) = onboard.directory(&description)?;
    let sector_id = first_enabled_sector(&entries)?;
    let original = onboard.read_sector(sector_id, description.sector_size)?;
    let mut replacement = original.clone();
    set_onboard_rate(&mut replacement, hz, description.profile_format_id)?;
    onboard.write_sector_verified(sector_id, &original, &replacement, false)?;
    print_onboard_result(
        OnboardWriteResult {
            device: target.index,
            name: target.name.clone(),
            operation: format!("set onboard report rate to {hz} Hz"),
            status: "verified".into(),
        },
        json,
    )
}

fn onboard_mode_get(index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let mode = mode_name(onboard.mode()?);
    if json {
        return print_json(&serde_json::json!({
            "device": target.index,
            "name": target.name,
            "mode": mode,
        }));
    }
    print_table(
        &["DEVICE", "NAME", "MODE"],
        &[vec![target.index.to_string(), target.name.clone(), mode]],
    );
    Ok(())
}

fn onboard_mode_set(onboard_mode: bool, index: Option<usize>, json: bool) -> Result<()> {
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    require_backup(&description)?;
    let mode = if onboard_mode { 0x01 } else { 0x02 };
    onboard.set_mode(mode)?;
    print_onboard_result(
        OnboardWriteResult {
            device: target.index,
            name: target.name.clone(),
            operation: format!("set mode to {}", mode_name(mode)),
            status: "verified".into(),
        },
        json,
    )
}

fn print_onboard_result(result: OnboardWriteResult, json: bool) -> Result<()> {
    if json {
        print_json(&result)
    } else {
        print_table(
            &["DEVICE", "NAME", "OPERATION", "STATUS"],
            &[vec![
                result.device.to_string(),
                result.name,
                result.operation,
                result.status,
            ]],
        );
        Ok(())
    }
}

fn print_button_rows(rows: &[ButtonRow]) {
    let table = rows
        .iter()
        .map(|row| {
            vec![
                row.button.clone(),
                if row.gshift { "yes" } else { "no" }.into(),
                row.binding.clone(),
                row.raw.clone(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["BUTTON", "G-SHIFT", "BINDING", "RAW"], &table);
}

fn mode_name(mode: u8) -> String {
    match mode {
        0x01 => "onboard".into(),
        0x02 => "host".into(),
        _ => format!("unknown (0x{mode:02X})"),
    }
}

fn device_kind(target: &ManagedDevice) -> &str {
    target.model.map_or("UNKNOWN", |model| model.kind.as_str())
}

fn hex_bytes(data: &[u8]) -> String {
    data.iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_u16_arg(value: &str) -> std::result::Result<u16, String> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u16::from_str_radix(hex, 16).map_err(|error| error.to_string())
    } else {
        value.parse::<u16>().map_err(|error| error.to_string())
    }
}

fn parse_u8_arg(value: &str) -> std::result::Result<u8, String> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u8::from_str_radix(hex, 16).map_err(|error| error.to_string())
    } else {
        value.parse::<u8>().map_err(|error| error.to_string())
    }
}

fn print_value_results<T>(results: &[ValueResult<T>], label: &str, json: bool) -> Result<()>
where
    T: Serialize + ToString,
{
    if json {
        return print_json(results);
    }
    let rows = results
        .iter()
        .map(|result| {
            vec![
                result.device.to_string(),
                result.name.clone(),
                result
                    .value
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "-".into()),
                result.error.clone().unwrap_or_default(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["DEVICE", "NAME", label, "ERROR"], &rows);
    Ok(())
}

fn selected_devices(discovery: &Discovery, index: Option<usize>) -> Result<Vec<&ManagedDevice>> {
    if discovery.devices.is_empty() {
        bail!("no Logitech HID++ devices were found");
    }
    match index {
        Some(index) => discovery
            .devices
            .iter()
            .find(|device| device.index == index)
            .map(|device| vec![device])
            .ok_or_else(|| anyhow::anyhow!("device index {index} was not found or is a receiver")),
        None => Ok(discovery.devices.iter().collect()),
    }
}

fn single_device(discovery: &Discovery, index: Option<usize>) -> Result<&ManagedDevice> {
    if let Some(index) = index {
        return discovery
            .devices
            .iter()
            .find(|device| device.index == index)
            .ok_or_else(|| anyhow::anyhow!("device index {index} was not found or is a receiver"));
    }
    match discovery.devices.as_slice() {
        [] => bail!("no Logitech HID++ devices were found"),
        [device] => Ok(device),
        devices => bail!(
            "{} devices were found; select one with --device <index>",
            devices.len()
        ),
    }
}

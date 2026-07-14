mod discovery;
mod hidpp;
mod output;
mod profile;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;

use discovery::{Discovery, ManagedDevice, discover, error_text};
use hidpp::device::{BatteryStatus, FeatureInfo};
use output::{print_json, print_table};
use profile::{
    Profile, default_ghub_db_path, default_store_path, import_ghub_database, load_store,
    merge_profiles, save_store,
};

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
    /// Import mouse settings from the G HUB settings database.
    ImportGhub {
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// List saved profiles.
    List,
    /// Apply a profile and read the effective values back from the device.
    Apply {
        name: String,
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
struct ProfileApplyResult {
    profile: String,
    device: usize,
    device_name: String,
    active_dpi: u16,
    report_rate_hz: u32,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::List => list(&discover_with_warnings()?, cli.json),
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
            ProfileCommand::ImportGhub { db } => profile_import_ghub(db, cli.json),
            ProfileCommand::List => profile_list(cli.json),
            ProfileCommand::Apply { name, device } => profile_apply(&name, device, cli.json),
        },
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
                row.status.clone(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["INDEX", "TYPE", "NAME", "WIRELESS PID", "STATUS"], &rows);
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

fn profile_import_ghub(db: Option<PathBuf>, json: bool) -> Result<()> {
    let db_path = match db {
        Some(path) => path,
        None => default_ghub_db_path()?,
    };
    let imported = import_ghub_database(&db_path)?;
    let store_path = default_store_path()?;
    let mut store = load_store(&store_path)?;
    merge_profiles(&mut store, &imported);
    save_store(&store_path, &store)?;
    print_profile_rows(&imported, json)
}

fn profile_list(json: bool) -> Result<()> {
    let store = load_store(&default_store_path()?)?;
    print_profile_rows(&store.profiles, json)
}

fn profile_apply(name: &str, index: Option<usize>, json: bool) -> Result<()> {
    let store = load_store(&default_store_path()?)?;
    let profile = store
        .profiles
        .iter()
        .find(|profile| profile.name == name)
        .with_context(|| format!("profile {name:?} was not found"))?;
    let discovery = discover_with_warnings()?;
    let target = single_device(&discovery, index)?;

    target
        .device
        .set_dpi(profile.active_dpi)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("failed to apply {} DPI", profile.active_dpi))?;
    target
        .device
        .set_report_rate(profile.report_rate_hz)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("failed to apply {} Hz report rate", profile.report_rate_hz))?;

    let active_dpi = target
        .device
        .dpi()
        .map_err(anyhow::Error::new)
        .context("DPI was set but could not be read back")?;
    let report_rate_hz = target
        .device
        .report_rate()
        .map_err(anyhow::Error::new)
        .context("report rate was set but could not be read back")?;
    let result = ProfileApplyResult {
        profile: profile.name.clone(),
        device: target.index,
        device_name: target.name.clone(),
        active_dpi,
        report_rate_hz,
    };

    if json {
        print_json(&result)
    } else {
        print_table(
            &["PROFILE", "DEVICE", "NAME", "ACTIVE DPI", "RATE (HZ)"],
            &[vec![
                result.profile,
                result.device.to_string(),
                result.device_name,
                result.active_dpi.to_string(),
                result.report_rate_hz.to_string(),
            ]],
        );
        Ok(())
    }
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

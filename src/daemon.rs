use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::bindings::{
    self, Action, AppBindings, Bindings, DeviceBindings, DpiValue, ProfileValue, parse_gkey,
};
use crate::discovery::{Discovery, ManagedDevice, discover};
use crate::gkeys::GKeys;
use crate::listener::{DecodedEvent, Listener, Notification};
use crate::live;
use crate::onboard::{Onboard, dpi_table, first_enabled_sector};
use crate::specialkeys::{SpecialKeyEvent, SpecialKeys, resolve_cid};
use crate::startup::{self, Startup, StartupDevice};

const ROUTE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const APP_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MUTEX_NAME: &str = "Local\\better-logihub-daemon";
const STOP_EVENT_NAME: &str = "Local\\better-logihub-daemon-stop";
const TASK_NAME: &str = "better-logihub daemon";
const LOG_CAP_BYTES: u64 = 5 * 1024 * 1024;
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static RESTORE_COMPLETE: AtomicBool = AtomicBool::new(false);

pub fn run(config_path: Option<PathBuf>, verbose: bool, json: bool) -> Result<()> {
    let path = absolute_path(config_path.map(Ok).unwrap_or_else(bindings::default_path)?)?;
    let _signals = SignalHandlers::install(false)?;
    let mut logger = DaemonLogger::console(json);
    run_core(&path, verbose, &mut logger, None)
}

pub fn run_resident(config_path: Option<PathBuf>, verbose: bool) -> Result<()> {
    let Some(_instance) = NamedMutex::acquire(MUTEX_NAME)? else {
        return Ok(());
    };
    let log_path = default_log_path()?;
    let mut logger = DaemonLogger::file(&log_path)?;
    let result = (|| {
        let path = absolute_path(config_path.map(Ok).unwrap_or_else(bindings::default_path)?)?;
        let _signals = SignalHandlers::install(true)?;
        let stop_event = StopEvent::create()?;
        let _state = DaemonStateGuard::write(&path)?;
        run_core(&path, verbose, &mut logger, Some(&stop_event))
    })();
    if let Err(error) = &result {
        logger.message("fatal", &format!("{error:#}"));
    }
    result
}

fn run_core(
    path: &Path,
    verbose: bool,
    logger: &mut DaemonLogger,
    stop_event: Option<&StopEvent>,
) -> Result<()> {
    let (bindings, created) = bindings::load_or_create(path)?;
    let startup_path = startup::default_path()?;
    let startup = startup::load_optional(&startup_path)?.unwrap_or_default();
    if created {
        logger.message(
            "config_created",
            &format!(
                "created example {}; edit it and start daemon again",
                path.display()
            ),
        );
        if startup.devices.is_empty() {
            return Ok(());
        }
    }

    let discovery = discover()?;
    for warning in &discovery.warnings {
        logger.message("warning", warning);
    }
    let mut session = DaemonSession::new(&discovery, &bindings, &startup, verbose)?;
    ensure!(
        !session.devices.is_empty(),
        "no connected or paired device matches selectors in {} or {}",
        path.display(),
        startup_path.display()
    );

    session.apply_all(false, logger);
    let targets = session
        .devices
        .iter()
        .map(|device| device.target)
        .collect::<Vec<_>>();
    let (mut listener, warnings) = Listener::new(&targets);
    for warning in warnings {
        logger.message("route_retry", &format!("{warning}; will retry"));
    }
    logger.message("started", &format!("using {}", path.display()));

    let mut next_refresh = Instant::now() + ROUTE_REFRESH_INTERVAL;
    let mut next_app_poll = Instant::now();
    while !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
        if stop_event.is_some_and(StopEvent::is_signaled) {
            SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
            continue;
        }
        if let Some(notification) = listener.next_event(Duration::from_millis(100))? {
            session.handle(notification, logger);
        }
        if Instant::now() >= next_app_poll {
            session.set_foreground_app(foreground_exe_name(), logger);
            next_app_poll = Instant::now() + APP_POLL_INTERVAL;
        }
        if Instant::now() >= next_refresh {
            let warnings = listener.refresh_routes();
            if verbose {
                for warning in warnings {
                    logger.message("route_retry", &warning);
                }
            }
            session.apply_all(false, logger);
            next_refresh = Instant::now() + ROUTE_REFRESH_INTERVAL;
        }
    }

    logger.message("stopping", "shutdown requested; restoring native input");
    session.restore(logger);
    logger.message("stopped", "restore sequence completed");
    Ok(())
}

pub fn watch(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let targets = selected_devices(discovery, index)?;
    let (mut listener, warnings) = Listener::new(&targets);
    for warning in warnings {
        eprintln!("warning: {warning}; listener will retry");
    }
    let _signals = SignalHandlers::install(false)?;
    if !json {
        println!("watching HID++ notifications; press Ctrl+C to stop");
    }
    let mut next_refresh = Instant::now() + ROUTE_REFRESH_INTERVAL;
    while !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
        if let Some(notification) = listener.next_event(Duration::from_millis(100))? {
            print_notification(&notification, json);
            let _ = io::stdout().flush();
        }
        if Instant::now() >= next_refresh {
            for warning in listener.refresh_routes() {
                eprintln!("warning: {warning}; listener will retry");
            }
            next_refresh = Instant::now() + ROUTE_REFRESH_INTERVAL;
        }
    }
    RESTORE_COMPLETE.store(true, Ordering::SeqCst);
    Ok(())
}

#[derive(Default)]
struct ResolvedActions {
    gkeys: BTreeMap<u8, Action>,
    gkeys_shifted: BTreeMap<u8, Action>,
    cids: BTreeMap<u16, Action>,
}

impl ResolvedActions {
    fn from_device(bindings: &DeviceBindings) -> Result<Self> {
        Ok(Self {
            gkeys: resolve_gkeys(&bindings.gkeys)?,
            gkeys_shifted: resolve_gkeys(&bindings.gkeys_shifted)?,
            cids: resolve_cids(&bindings.cids)?,
        })
    }

    fn from_app(bindings: &AppBindings) -> Result<Self> {
        Ok(Self {
            gkeys: resolve_gkeys(&bindings.gkeys)?,
            gkeys_shifted: resolve_gkeys(&bindings.gkeys_shifted)?,
            cids: resolve_cids(&bindings.cids)?,
        })
    }
}

fn resolve_gkeys(bindings: &BTreeMap<String, Action>) -> Result<BTreeMap<u8, Action>> {
    bindings
        .iter()
        .map(|(name, action)| Ok((parse_gkey(name)?, action.clone())))
        .collect()
}

fn resolve_cids(bindings: &BTreeMap<String, Action>) -> Result<BTreeMap<u16, Action>> {
    bindings
        .iter()
        .map(|(name, action)| Ok((resolve_cid(name)?, action.clone())))
        .collect()
}

struct ConfiguredDevice<'a> {
    target: &'a ManagedDevice,
    selector: String,
    actions: ResolvedActions,
    apps: BTreeMap<String, ResolvedActions>,
    gshift_key: Option<u8>,
    startup: Option<StartupDevice>,
    startup_applied: bool,
    gkeys_configured: bool,
    software_active: bool,
    configured_cids: BTreeSet<u16>,
    active_cids: BTreeSet<u16>,
    last_gkeys: u32,
    last_cids: BTreeSet<u16>,
    dpi_shift_original: Option<u16>,
    dpi_shift_inputs: BTreeSet<String>,
}

impl<'a> ConfiguredDevice<'a> {
    fn new(
        target: &'a ManagedDevice,
        binding_match: Option<(&str, &DeviceBindings)>,
        startup_match: Option<(&str, &StartupDevice)>,
    ) -> Result<Self> {
        let (binding_selector, device_bindings) = binding_match
            .map(|(selector, bindings)| (Some(selector), bindings.clone()))
            .unwrap_or_else(|| (None, DeviceBindings::default()));
        let actions = ResolvedActions::from_device(&device_bindings)?;
        let apps = device_bindings
            .apps
            .iter()
            .map(|(exe, bindings)| Ok((exe.clone(), ResolvedActions::from_app(bindings)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let gshift_key = device_bindings
            .gshift_key
            .as_deref()
            .map(parse_gkey)
            .transpose()?;
        let mut configured_cids = actions.cids.keys().copied().collect::<BTreeSet<_>>();
        for app in apps.values() {
            configured_cids.extend(app.cids.keys().copied());
        }
        let selector = [
            binding_selector,
            startup_match.map(|(selector, _)| selector),
        ]
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join("+");
        Ok(Self {
            target,
            selector,
            actions,
            apps,
            gshift_key,
            startup: startup_match.map(|(_, settings)| settings.clone()),
            startup_applied: false,
            gkeys_configured: false,
            software_active: false,
            configured_cids,
            active_cids: BTreeSet::new(),
            last_gkeys: 0,
            last_cids: BTreeSet::new(),
            dpi_shift_original: None,
            dpi_shift_inputs: BTreeSet::new(),
        })
    }

    fn has_gkey_bindings(&self) -> bool {
        self.gshift_key.is_some()
            || !self.actions.gkeys.is_empty()
            || !self.actions.gkeys_shifted.is_empty()
            || self
                .apps
                .values()
                .any(|app| !app.gkeys.is_empty() || !app.gkeys_shifted.is_empty())
    }

    fn desired_software_mode(&self) -> Option<bool> {
        if self.has_gkey_bindings() {
            Some(true)
        } else {
            self.startup
                .as_ref()
                .and_then(|settings| settings.gkeys_software_mode)
        }
    }

    fn highest_gkey(&self) -> Option<u8> {
        self.actions
            .gkeys
            .keys()
            .chain(self.actions.gkeys_shifted.keys())
            .chain(self.apps.values().flat_map(|app| app.gkeys.keys()))
            .chain(self.apps.values().flat_map(|app| app.gkeys_shifted.keys()))
            .copied()
            .chain(self.gshift_key)
            .max()
    }

    fn apply(&mut self, force: bool) -> Result<Vec<String>> {
        let mut applied = Vec::new();
        if let Some(enabled) = self.desired_software_mode()
            && (force || !self.gkeys_configured || self.software_active != enabled)
        {
            let gkeys = GKeys::new(&self.target.device)?;
            let count = gkeys.get_count()?;
            if let Some(highest) = self.highest_gkey() {
                ensure!(
                    highest <= count,
                    "configured G{highest}, but device reports only {count} G-keys"
                );
            }
            gkeys.enable_software_control(enabled)?;
            self.gkeys_configured = true;
            self.software_active = enabled;
            applied.push(format!(
                "G-key software mode {}",
                if enabled { "on" } else { "off" }
            ));
        }

        if !self.configured_cids.is_empty() {
            let keys = SpecialKeys::new(&self.target.device)?;
            let infos = keys.all_cid_info()?;
            for cid in self.configured_cids.iter().copied() {
                if !force && self.active_cids.contains(&cid) {
                    continue;
                }
                let info = infos
                    .iter()
                    .find(|info| info.cid == cid)
                    .with_context(|| format!("CID 0x{cid:04X} is not present on this device"))?;
                ensure!(
                    info.flags.divertable,
                    "CID 0x{cid:04X} ({}) is not divertable",
                    info.name
                );
                keys.set_reporting_raw(cid, 0x03, 0, 0)?;
                self.active_cids.insert(cid);
            }
            applied.push(format!("{} diverted CIDs", self.active_cids.len()));
        }

        if (force || !self.startup_applied)
            && let Some(settings) = &self.startup
        {
            applied.extend(startup::apply_device(self.target, settings, false)?);
            self.startup_applied = true;
        }
        Ok(applied)
    }

    fn gkey_action(&self, key: u8, shifted: bool, app: Option<&str>) -> Option<Action> {
        let overlay = app.and_then(|exe| self.apps.get(exe));
        resolve_gkey_action(&self.actions, overlay, key, shifted).cloned()
    }

    fn cid_action(&self, cid: u16, app: Option<&str>) -> Option<Action> {
        let overlay = app.and_then(|exe| self.apps.get(exe));
        resolve_overlay(&self.actions.cids, overlay.map(|app| &app.cids), &cid).cloned()
    }

    fn release_input(&mut self, input: &str) -> Result<()> {
        if self.dpi_shift_inputs.remove(input)
            && self.dpi_shift_inputs.is_empty()
            && let Some(original) = self.dpi_shift_original.take()
        {
            self.target
                .device
                .set_dpi(original)
                .map_err(anyhow::Error::new)?;
        }
        Ok(())
    }

    fn restore(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        self.dpi_shift_inputs.clear();
        if let Some(original) = self.dpi_shift_original.take()
            && let Err(error) = self.target.device.set_dpi(original)
        {
            errors.push(format!(
                "device {} temporary DPI restore: {error}",
                self.target.index
            ));
        }
        if self.software_active {
            match GKeys::new(&self.target.device)
                .and_then(|gkeys| gkeys.enable_software_control(false))
            {
                Ok(()) => self.software_active = false,
                Err(error) => errors.push(format!(
                    "device {} G-key software control: {error}",
                    self.target.index
                )),
            }
        }
        if !self.configured_cids.is_empty() {
            match SpecialKeys::new(&self.target.device) {
                Ok(keys) => {
                    for cid in self.configured_cids.iter().copied() {
                        // Clear only the volatile divert value; firmware-owned fields remain intact.
                        match keys.set_reporting_raw(cid, 0x02, 0, 0) {
                            Ok(()) => {
                                self.active_cids.remove(&cid);
                            }
                            Err(error) => errors.push(format!(
                                "device {} CID 0x{cid:04X}: {error}",
                                self.target.index
                            )),
                        }
                    }
                }
                Err(error) => errors.push(format!(
                    "device {} SpecialKeys restore: {error}",
                    self.target.index
                )),
            }
        }
        errors
    }
}

struct DaemonSession<'a> {
    devices: Vec<ConfiguredDevice<'a>>,
    active_app: Option<String>,
    verbose: bool,
    restored: bool,
}

impl<'a> DaemonSession<'a> {
    fn new(
        discovery: &'a Discovery,
        bindings: &Bindings,
        startup: &Startup,
        verbose: bool,
    ) -> Result<Self> {
        let mut devices = Vec::new();
        for target in &discovery.devices {
            let binding_match = matching_bindings(bindings, target);
            let startup_match = startup.matching(target);
            if binding_match.is_some() || startup_match.is_some() {
                devices.push(ConfiguredDevice::new(target, binding_match, startup_match)?);
            }
        }
        Ok(Self {
            devices,
            active_app: None,
            verbose,
            restored: false,
        })
    }

    fn apply_all(&mut self, force: bool, logger: &mut DaemonLogger) {
        for device in &mut self.devices {
            match device.apply(force) {
                Ok(applied) => logger.message(
                    "configured",
                    &format!(
                        "device {} ({}, selector {}): {}",
                        device.target.index,
                        device.target.name,
                        device.selector,
                        if applied.is_empty() {
                            "ready".into()
                        } else {
                            applied.join(", ")
                        }
                    ),
                ),
                Err(error) => logger.message(
                    "waiting",
                    &format!(
                        "device {} ({}) not configured yet: {error}",
                        device.target.index, device.target.name
                    ),
                ),
            }
        }
    }

    fn set_foreground_app(&mut self, app: Option<String>, logger: &mut DaemonLogger) {
        if self.active_app == app {
            return;
        }
        if self.verbose {
            logger.message(
                "app_switch",
                app.as_deref().unwrap_or("<unknown foreground process>"),
            );
        }
        self.active_app = app;
    }

    fn handle(&mut self, notification: Notification, logger: &mut DaemonLogger) {
        if self.verbose {
            logger.message(
                "raw_frame",
                &format!("device {}: {}", notification.device, notification.raw),
            );
        }
        logger.notification(&notification);
        let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.target.index == notification.device)
        else {
            return;
        };
        match notification.event {
            DecodedEvent::Gkeys { event } => {
                let pressed = event.held_mask & !device.last_gkeys;
                let released = device.last_gkeys & !event.held_mask;
                device.last_gkeys = event.held_mask;
                let shifted = device
                    .gshift_key
                    .is_some_and(|key| event.held_mask & (1_u32 << (key - 1)) != 0);
                for key in 1..=32 {
                    let bit = 1_u32 << (key - 1);
                    let input = format!("g{key}");
                    if pressed & bit != 0 {
                        let action = if device.gshift_key == Some(key as u8) {
                            None
                        } else {
                            device.gkey_action(key as u8, shifted, self.active_app.as_deref())
                        };
                        execute_binding(logger, device, &input, action.as_ref());
                    }
                    if released & bit != 0 {
                        release_binding(logger, device, &input);
                    }
                }
            }
            DecodedEvent::SpecialKeys {
                event: SpecialKeyEvent::DivertedButtons { held_cids },
            } => {
                let held = held_cids.into_iter().collect::<BTreeSet<_>>();
                for cid in held
                    .difference(&device.last_cids)
                    .copied()
                    .collect::<Vec<_>>()
                {
                    let input = format!("cid:0x{cid:04X}");
                    let action = device.cid_action(cid, self.active_app.as_deref());
                    execute_binding(logger, device, &input, action.as_ref());
                }
                for cid in device
                    .last_cids
                    .difference(&held)
                    .copied()
                    .collect::<Vec<_>>()
                {
                    release_binding(logger, device, &format!("cid:0x{cid:04X}"));
                }
                device.last_cids = held;
            }
            DecodedEvent::WirelessStatus {
                reconfigure,
                powered_on,
                ..
            } if reconfigure || powered_on => {
                logger.message(
                    "reconnect",
                    &format!(
                        "device {}; re-applying bindings and startup settings",
                        notification.device
                    ),
                );
                if let Err(error) = device.apply(true) {
                    logger.message(
                        "reapply_failed",
                        &format!("device {}: {error}", notification.device),
                    );
                }
            }
            _ => {}
        }
    }

    fn restore(&mut self, logger: &mut DaemonLogger) {
        if self.restored {
            return;
        }
        for device in &mut self.devices {
            for error in device.restore() {
                logger.message("restore_failed", &error);
            }
        }
        self.restored = true;
        RESTORE_COMPLETE.store(true, Ordering::SeqCst);
    }
}

impl Drop for DaemonSession<'_> {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        for device in &mut self.devices {
            let _ = device.restore();
        }
        RESTORE_COMPLETE.store(true, Ordering::SeqCst);
    }
}

fn matching_bindings<'a>(
    bindings: &'a Bindings,
    target: &ManagedDevice,
) -> Option<(&'a str, &'a DeviceBindings)> {
    bindings.devices.iter().find_map(|(selector, bindings)| {
        startup::selector_matches(selector, target).then_some((selector.as_str(), bindings))
    })
}

fn resolve_overlay<'a, K: Ord>(
    base: &'a BTreeMap<K, Action>,
    overlay: Option<&'a BTreeMap<K, Action>>,
    key: &K,
) -> Option<&'a Action> {
    overlay
        .and_then(|overlay| overlay.get(key))
        .or_else(|| base.get(key))
}

fn resolve_gkey_action<'a>(
    base: &'a ResolvedActions,
    overlay: Option<&'a ResolvedActions>,
    key: u8,
    shifted: bool,
) -> Option<&'a Action> {
    if shifted {
        resolve_overlay(
            &base.gkeys_shifted,
            overlay.map(|overlay| &overlay.gkeys_shifted),
            &key,
        )
    } else {
        resolve_overlay(&base.gkeys, overlay.map(|overlay| &overlay.gkeys), &key)
    }
}

fn execute_binding(
    logger: &mut DaemonLogger,
    device: &mut ConfiguredDevice<'_>,
    input: &str,
    action: Option<&Action>,
) {
    logger.edge(device.target.index, input, "press");
    let Some(action) = action else {
        return;
    };
    let result = execute_device_action(device, input, action);
    match result {
        Ok(()) => logger.message(
            "action",
            &format!(
                "device {} {input}: {}",
                device.target.index,
                action.description()
            ),
        ),
        Err(error) => logger.message(
            "action_failed",
            &format!("device {} {input}: {error}", device.target.index),
        ),
    }
}

fn release_binding(logger: &mut DaemonLogger, device: &mut ConfiguredDevice<'_>, input: &str) {
    if let Err(error) = device.release_input(input) {
        logger.message(
            "action_failed",
            &format!("device {} {input} release: {error}", device.target.index),
        );
    }
    logger.edge(device.target.index, input, "release");
}

fn execute_device_action(
    device: &mut ConfiguredDevice<'_>,
    input: &str,
    action: &Action,
) -> Result<()> {
    match action {
        Action::Dpi(action) => execute_dpi_action(device, input, &action.dpi),
        Action::Profile(action) => execute_profile_action(device.target, &action.profile),
        Action::Rgb(action) => {
            live::apply_rgb_setting(device.target, &action.rgb)?;
            Ok(())
        }
        Action::Brightness(action) => {
            live::set_brightness_percent(device.target, action.brightness)?;
            Ok(())
        }
        Action::PerKeyFill(action) => {
            live::apply_perkey_fill(device.target, None, &action.perkey_fill, false)?;
            Ok(())
        }
        _ => action.execute(),
    }
}

fn execute_dpi_action(
    device: &mut ConfiguredDevice<'_>,
    input: &str,
    action: &DpiValue,
) -> Result<()> {
    if let DpiValue::Value(value) = action {
        device
            .target
            .device
            .set_dpi(*value)
            .map_err(anyhow::Error::new)?;
        return Ok(());
    }
    let DpiValue::Named(name) = action else {
        unreachable!()
    };
    let table = active_dpi_table(device.target)?;
    let current = device.target.device.dpi().map_err(anyhow::Error::new)?;
    let current_index = table
        .levels
        .iter()
        .position(|value| *value == current)
        .unwrap_or_else(|| {
            table
                .levels
                .iter()
                .enumerate()
                .min_by_key(|(_, value)| value.abs_diff(current))
                .map(|(index, _)| index)
                .unwrap_or(0)
        });
    let index = match name.to_ascii_lowercase().as_str() {
        "up" => (current_index + 1).min(table.levels.len() - 1),
        "down" => current_index.saturating_sub(1),
        "cycle" => (current_index + 1) % table.levels.len(),
        "default" => usize::from(table.default_index),
        "shift" => {
            let shift = usize::from(
                table
                    .shift_index
                    .context("active profile has no shift DPI")?,
            );
            if device.dpi_shift_inputs.insert(input.into()) && device.dpi_shift_original.is_none() {
                device.dpi_shift_original = Some(current);
            }
            shift
        }
        _ => bail!("unknown DPI action {name:?}"),
    };
    device
        .target
        .device
        .set_dpi(table.levels[index])
        .map_err(anyhow::Error::new)?;
    Ok(())
}

fn active_dpi_table(target: &ManagedDevice) -> Result<crate::onboard::DpiTable> {
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    let (_, entries) = onboard.directory(&description)?;
    let current = onboard.current_profile()?;
    let sector = entries
        .iter()
        .find(|entry| entry.sector == current)
        .or_else(|| {
            entries
                .iter()
                .find(|entry| entry.index == usize::from(current))
        })
        .map(|entry| entry.sector)
        .or_else(|| first_enabled_sector(&entries).ok())
        .context("profile directory has no active profile")?;
    let bytes = onboard.read_sector(sector, description.sector_size)?;
    dpi_table(&bytes)
}

fn execute_profile_action(target: &ManagedDevice, action: &ProfileValue) -> Result<()> {
    let onboard = Onboard::new(&target.device)?;
    let description = onboard.description()?;
    let (_, entries) = onboard.directory(&description)?;
    let profiles = entries
        .iter()
        .filter(|entry| entry.enabled)
        .map(|entry| entry.sector)
        .collect::<Vec<_>>();
    ensure!(
        !profiles.is_empty(),
        "device has no enabled onboard profiles"
    );
    let current = onboard.current_profile()?;
    let profile = match action {
        ProfileValue::Number(number) => *number,
        ProfileValue::Named(name) if name.eq_ignore_ascii_case("next") => {
            let index = profiles
                .iter()
                .position(|profile| *profile == current)
                .map_or(0, |index| (index + 1) % profiles.len());
            profiles[index]
        }
        ProfileValue::Named(name) if name.eq_ignore_ascii_case("prev") => {
            let index = profiles
                .iter()
                .position(|profile| *profile == current)
                .map_or(profiles.len() - 1, |index| {
                    index.checked_sub(1).unwrap_or(profiles.len() - 1)
                });
            profiles[index]
        }
        ProfileValue::Named(name) => bail!("unknown profile action {name:?}"),
    };
    ensure!(
        profiles.contains(&profile),
        "profile 0x{profile:04X} is not enabled (available: {})",
        profiles
            .iter()
            .map(|profile| format!("0x{profile:04X}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    onboard.set_active_profile(profile)
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

struct DaemonLogger {
    output: LogOutput,
    json: bool,
}

enum LogOutput {
    Console,
    File(File),
}

impl DaemonLogger {
    fn console(json: bool) -> Self {
        Self {
            output: LogOutput::Console,
            json,
        }
    }

    fn file(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if fs::metadata(path).is_ok_and(|metadata| metadata.len() >= LOG_CAP_BYTES) {
            let rotated = path.with_file_name("daemon.log.1");
            match fs::remove_file(&rotated) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to remove {}", rotated.display()));
                }
            }
            fs::rename(path, &rotated).with_context(|| {
                format!(
                    "failed to rotate {} to {}",
                    path.display(),
                    rotated.display()
                )
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        Ok(Self {
            output: LogOutput::File(file),
            json: false,
        })
    }

    fn message(&mut self, kind: &str, message: &str) {
        if self.json {
            self.line(&serde_json::json!({"type":kind,"message":message}).to_string());
        } else if matches!(self.output, LogOutput::File(_)) {
            self.line(&format!("{kind}: {message}"));
        } else {
            self.line(message);
        }
    }

    fn edge(&mut self, device: usize, input: &str, edge: &str) {
        if self.json {
            self.line(
                &serde_json::json!({"type":"edge","device":device,"input":input,"edge":edge})
                    .to_string(),
            );
        } else {
            self.line(&format!("device {device} {input} {edge}"));
        }
    }

    fn notification(&mut self, notification: &Notification) {
        if self.json {
            match serde_json::to_string(notification) {
                Ok(value) => self.line(&value),
                Err(error) => self.message("serialize_failed", &error.to_string()),
            }
        } else {
            self.line(&format!(
                "device {} {} feature 0x{:04X} event {}: {:?}",
                notification.device,
                notification.name,
                notification.feature_id,
                notification.function,
                notification.event
            ));
        }
    }

    fn line(&mut self, value: &str) {
        match &mut self.output {
            LogOutput::Console => println!("{value}"),
            LogOutput::File(file) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                let _ = writeln!(
                    file,
                    "[{}.{:03}] {value}",
                    now.as_secs(),
                    now.subsec_millis()
                );
                let _ = file.flush();
            }
        }
    }
}

fn print_notification(notification: &Notification, json: bool) {
    if json {
        match serde_json::to_string(notification) {
            Ok(value) => println!("{value}"),
            Err(error) => eprintln!("failed to serialize notification: {error}"),
        }
    } else {
        println!(
            "device {} {} feature 0x{:04X} event {}: {:?}",
            notification.device,
            notification.name,
            notification.feature_id,
            notification.function,
            notification.event
        );
    }
}

pub fn default_log_path() -> Result<PathBuf> {
    Ok(app_dir()?.join("logs").join("daemon.log"))
}

fn state_path() -> Result<PathBuf> {
    Ok(app_dir()?.join("daemon-state.json"))
}

fn app_dir() -> Result<PathBuf> {
    let appdata = env::var_os("APPDATA").context("APPDATA is not set")?;
    Ok(PathBuf::from(appdata).join("better-logihub"))
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonStatus {
    pub task_name: String,
    pub task_installed: bool,
    pub daemon_running: bool,
    pub executable: Option<PathBuf>,
    pub config: PathBuf,
    pub log: PathBuf,
}

pub fn install(start: bool) -> Result<DaemonStatus> {
    let executable = sibling_daemon_path()?;
    ensure!(
        executable.is_file(),
        "{} does not exist; build both release binaries first",
        executable.display()
    );
    // HKCU\...\Run instead of a Task Scheduler ONLOGON task: schtasks /sc onlogon needs admin
    // rights, while a per-user Run value starts in the interactive session (SendInput works) as-is.
    let task_command = format!("\"{}\"", executable.display());
    let output = reg(&[
        "add",
        RUN_KEY,
        "/v",
        TASK_NAME,
        "/t",
        "REG_SZ",
        "/d",
        &task_command,
        "/f",
    ])?;
    ensure!(
        output.status.success(),
        "reg add failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    if start && !is_daemon_running()? {
        let mut command = Command::new(&executable);
        hide_child_console(&mut command);
        command
            .spawn()
            .with_context(|| format!("failed to start {}", executable.display()))?;
        for _ in 0..50 {
            if is_daemon_running()? {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    status()
}

pub fn uninstall() -> Result<DaemonStatus> {
    if task_exists()? {
        let output = reg(&["delete", RUN_KEY, "/v", TASK_NAME, "/f"])?;
        ensure!(
            output.status.success(),
            "reg delete failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if is_daemon_running()? {
        request_resident_stop()?;
        for _ in 0..100 {
            if !is_daemon_running()? {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        ensure!(
            !is_daemon_running()?,
            "daemon did not stop after the shutdown request"
        );
    }
    status()
}

pub fn status() -> Result<DaemonStatus> {
    let daemon_running = is_daemon_running()?;
    let state = if daemon_running {
        load_daemon_state().ok()
    } else {
        None
    };
    let executable = state
        .as_ref()
        .map(|state| state.executable.clone())
        .or_else(|| sibling_daemon_path().ok().filter(|path| path.is_file()));
    let config = state
        .map(|state| state.config)
        .unwrap_or(bindings::default_path()?);
    Ok(DaemonStatus {
        task_name: TASK_NAME.into(),
        task_installed: task_exists()?,
        daemon_running,
        executable,
        config,
        log: default_log_path()?,
    })
}

fn sibling_daemon_path() -> Result<PathBuf> {
    let executable = env::current_exe().context("failed to locate the running logihub.exe")?;
    Ok(executable.with_file_name("logihubd.exe"))
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

fn task_exists() -> Result<bool> {
    Ok(reg(&["query", RUN_KEY, "/v", TASK_NAME])?.status.success())
}

fn reg(args: &[&str]) -> Result<std::process::Output> {
    let mut command = Command::new("reg.exe");
    command.args(args);
    hide_child_console(&mut command);
    command.output().context("failed to run reg.exe")
}

#[cfg(windows)]
fn hide_child_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_child_console(_: &mut Command) {}

#[derive(Debug, Serialize, Deserialize)]
struct DaemonState {
    pid: u32,
    executable: PathBuf,
    config: PathBuf,
}

struct DaemonStateGuard {
    path: PathBuf,
}

impl DaemonStateGuard {
    fn write(config: &Path) -> Result<Self> {
        let path = state_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let state = DaemonState {
            pid: std::process::id(),
            executable: env::current_exe()?,
            config: config.to_owned(),
        };
        let mut bytes = serde_json::to_vec_pretty(&state)?;
        bytes.push(b'\n');
        fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for DaemonStateGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn load_daemon_state() -> Result<DaemonState> {
    let path = state_path()?;
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

struct SignalHandlers {
    _console: ConsoleHandler,
    _session: Option<SessionWindow>,
}

impl SignalHandlers {
    fn install(resident: bool) -> Result<Self> {
        SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        RESTORE_COMPLETE.store(false, Ordering::SeqCst);
        let console = ConsoleHandler::install()?;
        let session = resident.then(SessionWindow::install).transpose()?;
        Ok(Self {
            _console: console,
            _session: session,
        })
    }
}

struct ConsoleHandler;

impl ConsoleHandler {
    fn install() -> Result<Self> {
        install_console_handler()?;
        Ok(Self)
    }
}

impl Drop for ConsoleHandler {
    fn drop(&mut self) {
        RESTORE_COMPLETE.store(true, Ordering::SeqCst);
        uninstall_console_handler();
    }
}

fn wait_for_restore() {
    for _ in 0..100 {
        if RESTORE_COMPLETE.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn install_console_handler() -> Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    let result = unsafe { SetConsoleCtrlHandler(Some(console_handler), 1) };
    ensure!(
        result != 0,
        "SetConsoleCtrlHandler failed: {}",
        io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(not(windows))]
fn install_console_handler() -> Result<()> {
    bail!("daemon and watch signal handling are supported only on Windows")
}

#[cfg(windows)]
fn uninstall_console_handler() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe {
        SetConsoleCtrlHandler(Some(console_handler), 0);
    }
}

#[cfg(not(windows))]
fn uninstall_console_handler() {}

#[cfg(windows)]
unsafe extern "system" fn console_handler(control: u32) -> i32 {
    if !matches!(control, 0 | 1 | 2 | 5 | 6) {
        return 0;
    }
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    wait_for_restore();
    1
}

#[cfg(windows)]
struct NamedMutex {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl NamedMutex {
    fn acquire(name: &str) -> Result<Option<Self>> {
        use std::ptr;
        use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let name = wide(name);
        let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        ensure!(
            !handle.is_null(),
            "CreateMutexW failed: {}",
            io::Error::last_os_error()
        );
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            Ok(None)
        } else {
            Ok(Some(Self { handle }))
        }
    }
}

#[cfg(windows)]
impl Drop for NamedMutex {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

#[cfg(not(windows))]
struct NamedMutex;

#[cfg(not(windows))]
impl NamedMutex {
    fn acquire(_: &str) -> Result<Option<Self>> {
        bail!("resident daemon is supported only on Windows")
    }
}

#[cfg(windows)]
fn is_daemon_running() -> Result<bool> {
    use windows_sys::Win32::System::Threading::{MUTEX_ALL_ACCESS, OpenMutexW};

    let name = wide(MUTEX_NAME);
    let handle = unsafe { OpenMutexW(MUTEX_ALL_ACCESS, 0, name.as_ptr()) };
    if handle.is_null() {
        return Ok(false);
    }
    unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
    Ok(true)
}

#[cfg(not(windows))]
fn is_daemon_running() -> Result<bool> {
    Ok(false)
}

#[cfg(windows)]
struct StopEvent {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl StopEvent {
    fn create() -> Result<Self> {
        use std::ptr;
        use windows_sys::Win32::System::Threading::CreateEventW;

        let name = wide(STOP_EVENT_NAME);
        let handle = unsafe { CreateEventW(ptr::null(), 1, 0, name.as_ptr()) };
        ensure!(
            !handle.is_null(),
            "CreateEventW failed: {}",
            io::Error::last_os_error()
        );
        Ok(Self { handle })
    }

    fn is_signaled(&self) -> bool {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        unsafe { WaitForSingleObject(self.handle, 0) == WAIT_OBJECT_0 }
    }
}

#[cfg(windows)]
impl Drop for StopEvent {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

#[cfg(not(windows))]
struct StopEvent;

#[cfg(not(windows))]
impl StopEvent {
    fn create() -> Result<Self> {
        bail!("resident daemon is supported only on Windows")
    }

    fn is_signaled(&self) -> bool {
        false
    }
}

#[cfg(windows)]
fn request_resident_stop() -> Result<()> {
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};

    let name = wide(STOP_EVENT_NAME);
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    ensure!(
        !handle.is_null(),
        "daemon is running but its shutdown event is unavailable: {}",
        io::Error::last_os_error()
    );
    let result = unsafe { SetEvent(handle) };
    unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
    ensure!(
        result != 0,
        "SetEvent failed: {}",
        io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(not(windows))]
fn request_resident_stop() -> Result<()> {
    bail!("resident daemon is supported only on Windows")
}

#[cfg(windows)]
struct SessionWindow {
    hwnd: isize,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl SessionWindow {
    fn install() -> Result<Self> {
        use std::sync::mpsc;
        let (sender, receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || session_window_thread(sender));
        let hwnd = receiver
            .recv()
            .context("session-end window thread exited during startup")?
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            hwnd,
            thread: Some(thread),
        })
    }
}

#[cfg(windows)]
impl Drop for SessionWindow {
    fn drop(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
        unsafe {
            PostMessageW(self.hwnd as _, WM_CLOSE, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(not(windows))]
struct SessionWindow;

#[cfg(not(windows))]
impl SessionWindow {
    fn install() -> Result<Self> {
        bail!("session-end handling is supported only on Windows")
    }
}

#[cfg(windows)]
fn session_window_thread(sender: std::sync::mpsc::SyncSender<Result<isize, String>>) {
    use std::ptr;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, MSG, RegisterClassW, TranslateMessage,
        WNDCLASSW,
    };

    let class_name = wide("better-logihub-session-window");
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    if instance.is_null() {
        let _ = sender.send(Err(format!(
            "GetModuleHandleW failed: {}",
            io::Error::last_os_error()
        )));
        return;
    }
    let class = WNDCLASSW {
        lpfnWndProc: Some(session_window_proc),
        hInstance: instance,
        lpszClassName: class_name.as_ptr(),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        let _ = sender.send(Err(format!(
            "RegisterClassW failed: {}",
            io::Error::last_os_error()
        )));
        return;
    }
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        let _ = sender.send(Err(format!(
            "CreateWindowExW failed: {}",
            io::Error::last_os_error()
        )));
        return;
    }
    if sender.send(Ok(hwnd as isize)).is_err() {
        return;
    }
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(windows)]
unsafe extern "system" fn session_window_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, DestroyWindow, PostQuitMessage, WM_CLOSE, WM_DESTROY, WM_ENDSESSION,
        WM_QUERYENDSESSION,
    };

    match message {
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if wparam != 0 => {
            SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
            wait_for_restore();
            0
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(windows)]
fn foreground_exe_name() -> Option<String> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    let window = unsafe { GetForegroundWindow() };
    if window.is_null() {
        return None;
    }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, &mut pid) };
    if pid == 0 {
        return None;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) };
    unsafe { windows_sys::Win32::Foundation::CloseHandle(process) };
    if ok == 0 {
        return None;
    }
    let path = PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
    path.file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
}

#[cfg(not(windows))]
fn foreground_exe_name() -> Option<String> {
    None
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::specialkeys::ReportingUpdate;

    fn action(value: &str) -> Action {
        serde_json::from_str(value).unwrap()
    }

    #[test]
    fn reporting_update_used_by_daemon_is_volatile_only() {
        assert_eq!(
            ReportingUpdate {
                divert: Some(true),
                ..Default::default()
            }
            .encode_flags(),
            (0x03, 0)
        );
    }

    #[test]
    fn resolves_gshift_layer_without_falling_back_to_unshifted() {
        let mut base = ResolvedActions::default();
        base.gkeys.insert(1, action(r#"{"text":"base"}"#));
        base.gkeys_shifted
            .insert(2, action(r#"{"text":"shifted"}"#));
        assert!(resolve_gkey_action(&base, None, 1, false).is_some());
        assert!(resolve_gkey_action(&base, None, 1, true).is_none());
        assert_eq!(
            resolve_gkey_action(&base, None, 2, true)
                .unwrap()
                .description(),
            "text:7 chars"
        );
    }

    #[test]
    fn app_maps_overlay_the_matching_base_layer() {
        let mut base = ResolvedActions::default();
        base.gkeys.insert(1, action(r#"{"text":"base"}"#));
        base.gkeys.insert(2, action(r#"{"text":"base2"}"#));
        let mut app = ResolvedActions::default();
        app.gkeys.insert(1, action(r#"{"text":"app"}"#));
        assert_eq!(
            resolve_gkey_action(&base, Some(&app), 1, false)
                .unwrap()
                .description(),
            "text:3 chars"
        );
        assert_eq!(
            resolve_gkey_action(&base, Some(&app), 2, false)
                .unwrap()
                .description(),
            "text:5 chars"
        );
    }

    #[cfg(windows)]
    #[test]
    fn named_mutex_allows_only_one_guard() {
        let name = format!(
            "Local\\better-logihub-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let first = NamedMutex::acquire(&name).unwrap().unwrap();
        assert!(NamedMutex::acquire(&name).unwrap().is_none());
        drop(first);
        assert!(NamedMutex::acquire(&name).unwrap().is_some());
    }
}

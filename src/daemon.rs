use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::bindings::{self, Action, Bindings, DeviceBindings, parse_gkey};
use crate::discovery::{Discovery, ManagedDevice, discover};
use crate::gkeys::GKeys;
use crate::listener::{DecodedEvent, Listener, Notification};
use crate::specialkeys::{SpecialKeyEvent, SpecialKeys, resolve_cid};

const ROUTE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static RESTORE_COMPLETE: AtomicBool = AtomicBool::new(false);

pub fn run(config_path: Option<PathBuf>, verbose: bool, json: bool) -> Result<()> {
    let path = config_path.map(Ok).unwrap_or_else(bindings::default_path)?;
    let (bindings, created) = bindings::load_or_create(&path)?;
    if created {
        log_message(
            json,
            "config_created",
            &format!(
                "created example {}; edit it and start daemon again",
                path.display()
            ),
        );
        return Ok(());
    }

    let discovery = discover()?;
    for warning in &discovery.warnings {
        eprintln!("warning: {warning}");
    }
    let mut session = DaemonSession::new(&discovery, &bindings, verbose, json)?;
    ensure!(
        !session.devices.is_empty(),
        "no connected or paired device matches selectors in {}",
        path.display()
    );

    let _handler = ConsoleHandler::install()?;
    session.apply_all(false);
    let targets = session
        .devices
        .iter()
        .map(|device| device.target)
        .collect::<Vec<_>>();
    let (mut listener, warnings) = Listener::new(&targets);
    for warning in warnings {
        log_message(json, "route_retry", &format!("{warning}; will retry"));
    }
    log_message(json, "started", &format!("using {}", path.display()));

    let mut next_refresh = Instant::now() + ROUTE_REFRESH_INTERVAL;
    while !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
        if let Some(notification) = listener.next_event(Duration::from_millis(100))? {
            session.handle(notification);
        }
        if Instant::now() >= next_refresh {
            let warnings = listener.refresh_routes();
            if verbose {
                for warning in warnings {
                    log_message(json, "route_retry", &warning);
                }
            }
            session.apply_all(false);
            next_refresh = Instant::now() + ROUTE_REFRESH_INTERVAL;
        }
    }

    log_message(
        json,
        "stopping",
        "shutdown requested; restoring native input",
    );
    session.restore();
    RESTORE_COMPLETE.store(true, Ordering::SeqCst);
    log_message(json, "stopped", "restore sequence completed");
    Ok(())
}

pub fn watch(discovery: &Discovery, index: Option<usize>, json: bool) -> Result<()> {
    let targets = selected_devices(discovery, index)?;
    let (mut listener, warnings) = Listener::new(&targets);
    for warning in warnings {
        eprintln!("warning: {warning}; listener will retry");
    }
    let _handler = ConsoleHandler::install()?;
    if !json {
        println!("watching HID++ notifications; press Ctrl+C to stop");
    }
    let mut next_refresh = Instant::now() + ROUTE_REFRESH_INTERVAL;
    while !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
        if let Some(notification) = listener.next_event(Duration::from_millis(100))? {
            print_notification(&notification, json, false);
            // stdout is block-buffered when piped; events must show up immediately
            let _ = std::io::Write::flush(&mut std::io::stdout());
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

struct ConfiguredDevice<'a> {
    target: &'a ManagedDevice,
    selector: String,
    gkeys: BTreeMap<u8, Action>,
    cids: BTreeMap<u16, Action>,
    software_active: bool,
    active_cids: BTreeSet<u16>,
    last_gkeys: u32,
    last_cids: BTreeSet<u16>,
}

impl<'a> ConfiguredDevice<'a> {
    fn new(target: &'a ManagedDevice, selector: &str, bindings: &DeviceBindings) -> Result<Self> {
        let gkeys = bindings
            .gkeys
            .iter()
            .map(|(name, action)| Ok((parse_gkey(name)?, action.clone())))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let cids = bindings
            .cids
            .iter()
            .map(|(name, action)| Ok((resolve_cid(name)?, action.clone())))
            .collect::<Result<BTreeMap<_, _>>>()?;
        ensure!(
            !gkeys.is_empty() || !cids.is_empty(),
            "device selector {selector:?} has no G-key or CID bindings"
        );
        Ok(Self {
            target,
            selector: selector.into(),
            gkeys,
            cids,
            software_active: false,
            active_cids: BTreeSet::new(),
            last_gkeys: 0,
            last_cids: BTreeSet::new(),
        })
    }

    fn apply(&mut self, force: bool) -> Result<()> {
        if !self.gkeys.is_empty() && (force || !self.software_active) {
            let gkeys = GKeys::new(&self.target.device)?;
            let count = gkeys.get_count()?;
            if let Some(highest) = self.gkeys.keys().next_back() {
                ensure!(
                    *highest <= count,
                    "configured G{highest}, but device reports only {count} G-keys"
                );
            }
            gkeys.enable_software_control(true)?;
            self.software_active = true;
        }

        if !self.cids.is_empty() {
            if !force && self.active_cids.len() == self.cids.len() {
                return Ok(());
            }
            let keys = SpecialKeys::new(&self.target.device)?;
            let infos = keys.all_cid_info()?;
            for cid in self.cids.keys().copied() {
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
        }
        Ok(())
    }

    fn restore(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        if !self.gkeys.is_empty() {
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
        if !self.cids.is_empty() {
            match SpecialKeys::new(&self.target.device) {
                Ok(keys) => {
                    for cid in self.cids.keys().copied() {
                        // Only dvalid is set: clear volatile divert without altering
                        // persist/raw/remap/analytics fields owned by firmware or another app.
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
    verbose: bool,
    json: bool,
    restored: bool,
}

impl<'a> DaemonSession<'a> {
    fn new(
        discovery: &'a Discovery,
        bindings: &Bindings,
        verbose: bool,
        json: bool,
    ) -> Result<Self> {
        let mut devices = Vec::new();
        for target in &discovery.devices {
            if let Some((selector, device_bindings)) = matching_bindings(bindings, target) {
                devices.push(ConfiguredDevice::new(target, selector, device_bindings)?);
            }
        }
        Ok(Self {
            devices,
            verbose,
            json,
            restored: false,
        })
    }

    fn apply_all(&mut self, force: bool) {
        for device in &mut self.devices {
            match device.apply(force) {
                Ok(()) => log_message(
                    self.json,
                    "configured",
                    &format!(
                        "device {} ({}, selector {})",
                        device.target.index, device.target.name, device.selector
                    ),
                ),
                Err(error) => log_message(
                    self.json,
                    "waiting",
                    &format!(
                        "device {} ({}) not configured yet: {error}",
                        device.target.index, device.target.name
                    ),
                ),
            }
        }
    }

    fn handle(&mut self, notification: Notification) {
        if self.verbose {
            log_message(
                self.json,
                "raw_frame",
                &format!("device {}: {}", notification.device, notification.raw),
            );
        }
        print_notification(&notification, self.json, self.verbose);
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
                for key in 1..=32 {
                    let bit = 1_u32 << (key - 1);
                    if pressed & bit != 0 {
                        execute_binding(
                            self.json,
                            notification.device,
                            &format!("g{key}"),
                            device.gkeys.get(&(key as u8)),
                        );
                    }
                    if released & bit != 0 {
                        log_edge(
                            self.json,
                            notification.device,
                            &format!("g{key}"),
                            "release",
                        );
                    }
                }
            }
            DecodedEvent::SpecialKeys {
                event: SpecialKeyEvent::DivertedButtons { held_cids },
            } => {
                let held = held_cids.into_iter().collect::<BTreeSet<_>>();
                for cid in held.difference(&device.last_cids) {
                    execute_binding(
                        self.json,
                        notification.device,
                        &format!("cid:0x{cid:04X}"),
                        device.cids.get(cid),
                    );
                }
                for cid in device.last_cids.difference(&held) {
                    log_edge(
                        self.json,
                        notification.device,
                        &format!("cid:0x{cid:04X}"),
                        "release",
                    );
                }
                device.last_cids = held;
            }
            DecodedEvent::WirelessStatus {
                reconfigure,
                powered_on,
                ..
            } if reconfigure || powered_on => {
                log_message(
                    self.json,
                    "reconnect",
                    &format!("device {}; re-applying bindings", notification.device),
                );
                if let Err(error) = device.apply(true) {
                    log_message(
                        self.json,
                        "reapply_failed",
                        &format!("device {}: {error}", notification.device),
                    );
                }
            }
            _ => {}
        }
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        for device in &mut self.devices {
            let errors = device.restore();
            for error in errors {
                log_message(self.json, "restore_failed", &error);
            }
        }
        self.restored = true;
        RESTORE_COMPLETE.store(true, Ordering::SeqCst);
    }
}

impl Drop for DaemonSession<'_> {
    fn drop(&mut self) {
        self.restore();
    }
}

fn matching_bindings<'a>(
    bindings: &'a Bindings,
    target: &ManagedDevice,
) -> Option<(&'a str, &'a DeviceBindings)> {
    bindings.devices.iter().find_map(|(selector, bindings)| {
        let model_match = target
            .model
            .is_some_and(|model| model.model_id.eq_ignore_ascii_case(selector));
        let pid_match = target
            .pid
            .is_some_and(|pid| parse_pid(selector) == Some(pid));
        (model_match || pid_match).then_some((selector.as_str(), bindings))
    })
}

fn parse_pid(value: &str) -> Option<u16> {
    let value = value.trim();
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))?;
    u16::from_str_radix(value, 16).ok()
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

fn execute_binding(json: bool, device: usize, input: &str, action: Option<&Action>) {
    log_edge(json, device, input, "press");
    let Some(action) = action else {
        return;
    };
    match action.execute() {
        Ok(()) => log_message(
            json,
            "action",
            &format!("device {device} {input}: {}", action.description()),
        ),
        Err(error) => log_message(
            json,
            "action_failed",
            &format!("device {device} {input}: {error}"),
        ),
    }
}

fn log_edge(json: bool, device: usize, input: &str, edge: &str) {
    if json {
        println!(
            "{}",
            serde_json::json!({"type":"edge","device":device,"input":input,"edge":edge})
        );
    } else {
        println!("device {device} {input} {edge}");
    }
}

fn print_notification(notification: &Notification, json: bool, _raw_already_printed: bool) {
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

fn log_message(json: bool, kind: &str, message: &str) {
    if json {
        println!("{}", serde_json::json!({"type":kind,"message":message}));
    } else {
        println!("{message}");
    }
}

struct ConsoleHandler;

impl ConsoleHandler {
    fn install() -> Result<Self> {
        SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        RESTORE_COMPLETE.store(false, Ordering::SeqCst);
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

#[cfg(windows)]
fn install_console_handler() -> Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    let result = unsafe { SetConsoleCtrlHandler(Some(console_handler), 1) };
    ensure!(
        result != 0,
        "SetConsoleCtrlHandler failed: {}",
        std::io::Error::last_os_error()
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
    // Close/logoff/shutdown handlers are given a short grace period by Windows.
    // Keep this callback alive while the main loop restores volatile diverts.
    for _ in 0..100 {
        if RESTORE_COMPLETE.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::specialkeys::ReportingUpdate;

    #[test]
    fn matches_hex_pid_syntax() {
        assert_eq!(parse_pid("0x407c"), Some(0x407C));
        assert_eq!(parse_pid("407c"), None);
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
}

use std::collections::HashSet;
use std::env;
use std::fs;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use crate::algos::{self, Algo};
use crate::config::{load_text, parse_devices, parse_region_map, parse_thermal, parse_thermal_map};
use crate::devices::{is_cpu_name, DeviceStore};
use crate::sysfs::RawCache;
use crate::thermal_sensors::SensorStore;

#[derive(Clone, Debug)]
pub struct Options {
    pub config_dir: PathBuf,
    pub config: Option<PathBuf>,
    pub map: Option<PathBuf>,
    pub region_map: Option<PathBuf>,
    pub devices: PathBuf,
    pub sysfs_root: PathBuf,
    pub dry_run: bool,
    pub once: bool,
    pub verbose: bool,
    pub raw_cache: Option<Arc<RawCache>>,
}

pub type Args = Options;

impl Default for Options {
    fn default() -> Self {
        Self {
            config_dir: env_path("MI_THERMALD_CONFIG_DIR").unwrap_or_else(default_config_dir),
            config: env_path("MI_THERMALD_CONFIG"),
            map: env_path("MI_THERMALD_MAP"),
            region_map: env_path("MI_THERMALD_REGION_MAP"),
            devices: env_path("MI_THERMALD_DEVICES")
                .unwrap_or_else(|| PathBuf::from("/vendor/etc/thermald-devices.conf")),
            sysfs_root: env_path("MI_THERMALD_SYSFS_ROOT").unwrap_or_else(|| PathBuf::from("/")),
            dry_run: env_flag("MI_THERMALD_DRY_RUN"),
            once: false,
            verbose: cfg!(feature = "debug-log") || env_flag("MI_THERMALD_VERBOSE"),
            raw_cache: None,
        }
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn env_flag(name: &str) -> bool {
    matches!(
        env::var(name).ok().as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

pub struct Engine {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Engine {
    pub fn start(args: Options) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let config_path = initial_config_path(&args);

        if args.once {
            let runtime = build_runtime(&args, &config_path)?;
            algos::run_once(
                runtime.algos,
                &runtime.sensors,
                &runtime.devices,
                args.dry_run,
            );
            return Ok(Self { stop, handle: None });
        }

        let runtime = build_runtime(&args, &config_path)?;
        let thread_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("mi-thermald".into())
            .spawn(move || {
                if let Err(error) = watch(args, runtime, thread_stop) {
                    eprintln!("mi_thermald: {error}");
                }
            })
            .map_err(|error| format!("spawn mi_thermald: {error}"))?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Runtime {
    sensors: SensorStore,
    devices: DeviceStore,
    algos: Vec<Algo>,
}

fn reset_cpu_limits(sysfs_root: &Path) {
    let path = sysfs_root.join("sys/class/thermal/thermal_message/cpu_limits");
    if path.exists() {
        for cpu in ["cpu0", "cpu3", "cpu7"] {
            let _ = fs::write(&path, format!("{cpu} 2147483647"));
        }
    }
}

fn watch(args: Options, runtime: Runtime, stop: Arc<AtomicBool>) -> Result<(), String> {
    if !args.dry_run {
        reset_cpu_limits(&args.sysfs_root);
    }
    let mut sensors = Arc::new(runtime.sensors);
    let mut devices = Arc::new(runtime.devices);
    let mut group = algos::start(
        runtime.algos,
        Arc::clone(&sensors),
        Arc::clone(&devices),
        args.dry_run,
        args.verbose,
    );

    let sconfig = args
        .sysfs_root
        .join("sys/class/thermal/thermal_message/sconfig");
    if !sconfig.exists() {
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
        }
        group.stop();
        return Ok(());
    }

    let mut mode_watcher = ColorOsModeWatcher::new(&args.sysfs_root);
    let initial_mode = mode_watcher.poll();
    if !args.dry_run {
        let current = read_i64(&sconfig).unwrap_or(-1);
        let desired = match initial_mode {
            Some(mode) => Some(mode.sconfig()),
            None if current < 0 => Some(0),
            None => None,
        };
        if let Some(target) = desired {
            if current != target {
                if let Err(err) = fs::write(&sconfig, format!("{target}\n")) {
                    crate::klog(&format!("failed to initialize sconfig={target}: {err}"));
                } else {
                    crate::klog(&format!("initialized sconfig={target} (was {current})"));
                }
            }
        }
    }

    let region_map = region_map_path(&args);
    // Startup loads the default config (sconfig=0); keep `last = 0` so any
    // non-zero sconfig (e.g. 6=nolimits, 20=yuanshen, 5=phone) switches immediately.
    let mut last: i64 = 0;
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(500));
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if !args.dry_run {
            if let Some(mode) = mode_watcher.poll() {
                let target = mode.sconfig();
                if read_i64(&sconfig) != Some(target) {
                    if let Err(err) = fs::write(&sconfig, format!("{target}\n")) {
                        crate::klog(&format!("coloros mode {:?} -> sconfig={target} failed: {err}", mode));
                    } else {
                        crate::klog(&format!("coloros mode {:?} -> sconfig={target}", mode));
                    }
                }
            }
        }
        let Some(value) = read_i64(&sconfig) else {
            continue;
        };
        if value == last {
            continue;
        }
        last = value;
        if !(0..=0x800).contains(&value) {
            continue;
        }
        let resolved = match region_map.as_deref() {
            Some(map) => match resolve_config_with_map(&args, value, Some(map)) {
                Ok(Some(path)) => Ok(Some(path)),
                Ok(None) => resolve_config(&args, value),
                Err(error) => Err(error),
            },
            None => resolve_config(&args, value),
        };
        match resolved {
            Ok(Some(path)) => match build_runtime(&args, &path) {
                Ok(runtime) => {
                    crate::klog(&format!(
                        "switched thermal config sconfig={value} -> {}",
                        path.display()
                    ));
                    if args.verbose {
                        println!("switch config sconfig={value} path={}", path.display());
                    }
                    group.stop();
                    let active_devices: HashSet<&str> = runtime
                        .algos
                        .iter()
                        .filter(|algo| !algo.disable)
                        .flat_map(|algo| algo.devices.iter().map(String::as_str))
                        .collect();
                    runtime
                        .devices
                        .reset_unused_from(&devices, &active_devices, args.dry_run);
                    if !args.dry_run {
                        reset_cpu_limits(&args.sysfs_root);
                    }
                    runtime.sensors.copy_state_from(&sensors);
                    let Runtime {
                        algos,
                        sensors: new_sensors,
                        devices: new_devices,
                        ..
                    } = runtime;
                    let new_sensors = Arc::new(new_sensors);
                    let new_devices = Arc::new(new_devices);
                    group = algos::start(
                        algos,
                        Arc::clone(&new_sensors),
                        Arc::clone(&new_devices),
                        args.dry_run,
                        args.verbose,
                    );
                    sensors = new_sensors;
                    devices = new_devices;
                }
                Err(error) => eprintln!("switch config {}: {error}", path.display()),
            },
            Ok(None) => {
                if args.verbose {
                    println!("switch config sconfig={value}: no entry");
                }
            }
            Err(error) => eprintln!("switch config sconfig={value}: {error}"),
        }
    }

    group.stop();
    Ok(())
}

fn initial_config_path(args: &Args) -> PathBuf {
    if let Some(config) = &args.config {
        return config.clone();
    }
    if bootmode() == "charger" {
        let vendor_normal = Path::new("/vendor/etc/thermal-normal.conf").exists();
        if !vendor_normal {
            return PathBuf::from("/odm/etc/thermal-chg-only.conf");
        }
        let index = read_i64(
            &args
                .sysfs_root
                .join("sys/class/thermal/thermal_message/charger_only_index"),
        )
        .unwrap_or(-1);
        let name = if index > 0 {
            format!("thermal-chg-only-{index}.conf")
        } else {
            "thermal-chg-only.conf".to_owned()
        };
        return PathBuf::from("/vendor/etc").join(name);
    }
    let mode = fs::read_to_string("/data/vendor/thermal/thermal-global-mode")
        .ok()
        .and_then(|text| text.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if let Some(region_map) = region_map_path(args) {
        if let Ok(Some(path)) = resolve_config_with_map(args, mode, Some(&region_map)) {
            return path;
        }
    }
    if let Ok(Some(path)) = resolve_config(args, mode) {
        return path;
    }
    args.config_dir.join("thermal-normal.conf")
}

fn region_map_path(args: &Args) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = &args.region_map {
        candidates.push(path.clone());
    }
    candidates.extend([
        PathBuf::from("/odm/etc/thermal-region-map.conf"),
        PathBuf::from("/vendor/etc/thermal-region-map.conf"),
    ]);
    let map_path = candidates.into_iter().find(|path| path.exists())?;
    let text = load_text(&map_path).ok()?;
    let version = env::var("MI_THERMALD_HWVERSION")
        .ok()
        .or_else(|| property("ro.boot.hwversion"))
        .unwrap_or_else(|| "unknown".to_owned());
    let entries = parse_region_map(&text);
    for (prefix, name) in entries {
        if version.starts_with(&prefix) {
            return resolve_named_config(args, &name);
        }
    }
    None
}

fn bootmode() -> String {
    env::var("MI_THERMALD_BOOTMODE")
        .ok()
        .or_else(|| property("ro.bootmode"))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn property(name: &str) -> Option<String> {
    for command in ["/system/bin/getprop", "getprop"] {
        let Ok(output) = Command::new(command).arg(name).output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

fn build_runtime(args: &Args, config_path: &Path) -> Result<Runtime, String> {
    let thermal_text = load_text(config_path)?;
    let thermal = parse_thermal(&thermal_text);

    let device_configs = match load_text(&args.devices) {
        Ok(text) => parse_devices(&text),
        Err(_) => Vec::new(),
    };

    let raw_cache = args.raw_cache.clone().unwrap_or_default();
    let sensors = SensorStore::with_cache(&args.sysfs_root, &thermal, raw_cache)?;
    let mut devices = DeviceStore::new(&args.sysfs_root, &device_configs);
    let mut algos = Algo::from_config(&thermal);

    for algo in &algos {
        for device in &algo.devices {
            if is_cpu_name(device) || device.starts_with("VIRTUAL-SENSOR") {
                devices.ensure(&args.sysfs_root, device);
            }
        }
    }

    let configured = algos.len();
    algos.retain(|algo| {
        sensors.contains(&algo.sensor) && algo.devices.iter().all(|device| devices.contains(device))
    });

    if args.verbose {
        println!(
            "config={} sensors={} devices={} algos={}",
            config_path.display(),
            sensors.names().len(),
            algos.iter().map(|algo| algo.devices.len()).sum::<usize>(),
            algos.len()
        );
        if algos.len() != configured {
            println!("skipped {} invalid algos", configured - algos.len());
        }
        for algo in &algos {
            println!(
                "{} {:?} sensor={} devices={:?} polling={}ms",
                algo.name, algo.kind, algo.sensor, algo.devices, algo.polling_ms
            );
        }
    }

    Ok(Runtime {
        sensors,
        devices,
        algos,
    })
}

fn resolve_config(args: &Args, sconfig: i64) -> Result<Option<PathBuf>, String> {
    resolve_config_with_map(args, sconfig, None)
}

fn resolve_config_with_map(
    args: &Args,
    sconfig: i64,
    map_override: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    let mut map_path = None;
    let mut candidates = Vec::new();
    if let Some(map) = map_override {
        candidates.push(map.to_path_buf());
    } else if let Some(map) = &args.map {
        candidates.push(map.clone());
    }
    if map_override.is_none() {
        candidates.extend([
            args.config_dir.join("thermal-map.conf"),
            PathBuf::from("/odm/etc/thermal-map.conf"),
            PathBuf::from("/vendor/etc/thermal-map.conf"),
        ]);
    }
    for candidate in candidates {
        if candidate.exists() {
            map_path = Some(candidate);
            break;
        }
    }
    let Some(map_path) = map_path else {
        return Ok(None);
    };
    let entries = parse_thermal_map(&load_text(&map_path)?);
    let Some((_, name)) = entries.into_iter().find(|(index, _)| *index == sconfig) else {
        return Ok(None);
    };
    Ok(resolve_named_config(args, &name))
}

fn resolve_named_config(args: &Args, name: &str) -> Option<PathBuf> {
    [
        PathBuf::from("/data/vendor/thermal/config").join(name),
        args.config_dir.join(name),
        PathBuf::from("/odm/etc").join(name),
        PathBuf::from("/vendor/etc").join(name),
    ]
    .into_iter()
    .find(|candidate| candidate.exists())
}

fn read_i64(path: &Path) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse::<i64>().ok()
}

fn default_config_dir() -> PathBuf {
    if PathBuf::from("/odm/etc/thermal-normal.conf").exists() {
        PathBuf::from("/odm/etc")
    } else if PathBuf::from("/vendor/etc/thermal-normal.conf").exists() {
        PathBuf::from("/vendor/etc")
    } else {
        PathBuf::from("/odm/etc")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColorOsPowerMode {
    PowerSaving,
    Balanced,
    HighPerformance,
    ProGamer,
}

impl ColorOsPowerMode {
    fn sconfig(self) -> i64 {
        match self {
            Self::PowerSaving => 5,      // thermal-phone.conf (42C)
            Self::Balanced => 0,         // thermal-normal.conf (46C)
            Self::HighPerformance => 20, // thermal-yuanshen.conf (48C, no early CPU throttle)
            Self::ProGamer => 6,         // thermal-nolimits.conf (51C, full uncapped gaming)
        }
    }
}

struct ColorOsModeWatcher {
    system_xml: PathBuf,
    global_xml: PathBuf,
    inotify_fd: Option<RawFd>,
    last_system_mtime: Option<SystemTime>,
    last_global_mtime: Option<SystemTime>,
    high_perf: bool,
    gt_mode: bool,
    low_power: bool,
    last_mode: Option<ColorOsPowerMode>,
}

impl Drop for ColorOsModeWatcher {
    fn drop(&mut self) {
        if let Some(fd) = self.inotify_fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

impl ColorOsModeWatcher {
    fn new(root: &Path) -> Self {
        let dir = root.join("data/system/users/0");
        let inotify_fd = open_settings_inotify(&dir);
        Self {
            system_xml: dir.join("settings_system.xml"),
            global_xml: dir.join("settings_global.xml"),
            inotify_fd,
            last_system_mtime: None,
            last_global_mtime: None,
            high_perf: false,
            gt_mode: false,
            low_power: false,
            last_mode: None,
        }
    }

    /// Returns `Some(mode)` only when ColorOS power/performance mode changes.
    /// Uses inotify so steady-state polling performs zero `stat()` calls.
    fn poll(&mut self) -> Option<ColorOsPowerMode> {
        let initial = self.last_mode.is_none();
        if !initial {
            if let Some(fd) = self.inotify_fd {
                if !drain_inotify(fd) {
                    return None;
                }
            }
        }

        let mut changed = false;

        if let Ok(mtime) = fs::metadata(&self.system_xml).and_then(|m| m.modified()) {
            if self.last_system_mtime != Some(mtime) {
                self.last_system_mtime = Some(mtime);
                if let Ok(bytes) = fs::read(&self.system_xml) {
                    let hp = abx_setting_value(&bytes, "high_performance_mode_on")
                        .as_deref()
                        == Some("1");
                    let gt = abx_setting_value(&bytes, "gt_mode_state_setting")
                        .as_deref()
                        == Some("1");
                    self.high_perf = hp;
                    self.gt_mode = gt;
                    changed = true;
                }
            }
        }

        if let Ok(mtime) = fs::metadata(&self.global_xml).and_then(|m| m.modified()) {
            if self.last_global_mtime != Some(mtime) {
                self.last_global_mtime = Some(mtime);
                if let Ok(bytes) = fs::read(&self.global_xml) {
                    self.low_power =
                        abx_setting_value(&bytes, "low_power").as_deref() == Some("1");
                    changed = true;
                }
            }
        }

        if !changed && self.last_mode.is_some() {
            return None;
        }
        if self.last_system_mtime.is_none() && self.last_global_mtime.is_none() {
            return None;
        }

        let mode = if self.low_power {
            ColorOsPowerMode::PowerSaving
        } else if self.gt_mode {
            ColorOsPowerMode::ProGamer
        } else if self.high_perf {
            ColorOsPowerMode::HighPerformance
        } else {
            ColorOsPowerMode::Balanced
        };

        if self.last_mode == Some(mode) {
            None
        } else {
            self.last_mode = Some(mode);
            Some(mode)
        }
    }
}

pub(crate) fn open_settings_inotify(dir: &Path) -> Option<RawFd> {
    let c_dir = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let wd = unsafe {
        libc::inotify_add_watch(
            fd,
            c_dir.as_ptr(),
            libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE,
        )
    };
    if wd < 0 {
        unsafe {
            libc::close(fd);
        }
        return None;
    }
    Some(fd)
}

pub(crate) fn drain_inotify(fd: RawFd) -> bool {
    let mut buf = [0u8; 512];
    let mut saw_event = false;
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        saw_event = true;
    }
    saw_event
}

/// Extract a setting's string value from an Android Binary XML (ABX) settings file.
/// Layout: [u16_be key_len][key bytes] [0x2f (ATTR_STRING)] [u16_be intern_id | 0xffff u16_be len name] [u16_be val_len][val bytes]
pub(crate) fn abx_setting_value(data: &[u8], key: &str) -> Option<String> {
    let key_bytes = key.as_bytes();
    let key_len = u16::try_from(key_bytes.len()).ok()?;
    let mut needle = Vec::with_capacity(2 + key_bytes.len());
    needle.extend_from_slice(&key_len.to_be_bytes());
    needle.extend_from_slice(key_bytes);

    let mut offset = 0;
    while offset + needle.len() <= data.len() {
        let rel = data[offset..]
            .windows(needle.len())
            .position(|w| w == needle.as_slice())?;
        let after = offset + rel + needle.len();
        if after < data.len() && data[after] == 0x2f {
            let mut pos = after + 1;
            if pos + 2 <= data.len() {
                let id = u16::from_be_bytes([data[pos], data[pos + 1]]);
                pos += 2;
                if id == 0xffff {
                    if pos + 2 > data.len() {
                        return None;
                    }
                    let name_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                    pos += 2 + name_len;
                }
                if pos + 2 <= data.len() {
                    let val_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                    pos += 2;
                    if pos + val_len <= data.len() {
                        return String::from_utf8(data[pos..pos + val_len].to_vec()).ok();
                    }
                }
            }
        }
        offset = after;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_abx_setting_value() {
        // Real ABX slice from peridot settings_system.xml: high_performance_mode_on = "1"
        let sample: &[u8] = &[
            0x2f, 0x00, 0x04, 0x00, 0x18, b'h', b'i', b'g', b'h', b'_', b'p', b'e', b'r', b'f',
            b'o', b'r', b'm', b'a', b'n', b'c', b'e', b'_', b'm', b'o', b'd', b'e', b'_', b'o',
            b'n', 0x2f, 0x00, 0x05, 0x00, 0x01, b'1', 0x2f, 0x00, 0x06,
        ];
        assert_eq!(
            abx_setting_value(sample, "high_performance_mode_on").as_deref(),
            Some("1")
        );
        assert_eq!(abx_setting_value(sample, "low_power"), None);
    }
}

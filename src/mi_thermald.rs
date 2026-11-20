use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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

fn watch(args: Options, runtime: Runtime, stop: Arc<AtomicBool>) -> Result<(), String> {
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

    let mut last = read_i64(&sconfig).unwrap_or(-1);
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(500));
        if stop.load(Ordering::Relaxed) {
            break;
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
        match resolve_config(&args, value) {
            Ok(Some(path)) => match build_runtime(&args, &path) {
                Ok(runtime) => {
                    if args.verbose {
                        println!("switch config sconfig={value} path={}", path.display());
                    }
                    group.stop();
                    runtime.devices.copy_state_from(&devices);
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

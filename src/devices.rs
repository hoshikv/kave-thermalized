use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::DeviceConfig;
use crate::thermal_sensors::resolve;

type CachedFile = Arc<Mutex<Option<File>>>;
type SysfsCache = HashMap<PathBuf, CachedFile>;

#[derive(Debug)]
pub struct Device {
    pub path: Option<PathBuf>,
    pub levels: Arc<[i64]>,
    pub def_target: i64,
    pub select_higher: bool,
    pub limit: Option<i64>,
    pub last: Option<i64>,
    last_applied: Option<i64>,
    write_name: Option<String>,
    requests: HashMap<u32, i64>,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceStore {
    root: PathBuf,
    devices: HashMap<String, Arc<Mutex<Device>>>,
    sysfs: Arc<Mutex<SysfsCache>>,
}

impl DeviceStore {
    pub fn new(root: &Path, configs: &[DeviceConfig]) -> Self {
        let cooling = scan_cooling_devices(root);
        let mut devices = HashMap::new();

        for config in configs {
            let (path, write_name) = resolve_device(root, config, &cooling);
            let levels = if is_cpu_name(&config.name) {
                read_cpu_levels(root, &config.name)
            } else {
                Vec::new()
            };
            let device = Device {
                path,
                levels: Arc::from(levels),
                def_target: config.def_target,
                select_higher: config.select_higher,
                limit: None,
                last: None,
                last_applied: None,
                write_name,
                requests: HashMap::new(),
            };
            devices.insert(config.name.clone(), Arc::new(Mutex::new(device)));
        }

        Self {
            root: root.to_path_buf(),
            devices,
            sysfs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn ensure(&mut self, root: &Path, name: &str) {
        if self.devices.contains_key(name) {
            return;
        }
        let cooling = scan_cooling_devices(root);
        let config = DeviceConfig {
            name: name.to_owned(),
            cooling_name: Some(name.to_owned()),
            path: None,
            def_target: 0,
            select_higher: true,
        };
        let (path, write_name) = resolve_device(root, &config, &cooling);
        let cpu = is_cpu_name(name);
        let levels: Arc<[i64]> = Arc::from(if cpu {
            read_cpu_levels(root, name)
        } else {
            Vec::new()
        });
        let def_target = if cpu {
            let current = read_i64_path(
                &root
                    .join("sys/devices/system/cpu")
                    .join(name)
                    .join("cpufreq/scaling_max_freq"),
            )
            .unwrap_or(0);
            current.max(levels.first().copied().unwrap_or(0))
        } else if name.starts_with("VIRTUAL-SENSOR") {
            1000
        } else {
            0
        };
        let device = Device {
            path,
            levels,
            def_target,
            select_higher: !cpu,
            limit: None,
            last: None,
            last_applied: None,
            write_name,
            requests: HashMap::new(),
        };
        self.devices
            .insert(name.to_owned(), Arc::new(Mutex::new(device)));
    }

    pub fn reset_unused_from(
        &self,
        old: &DeviceStore,
        active_devices: &HashSet<&str>,
        dry_run: bool,
    ) {
        for (name, old_device) in &old.devices {
            if active_devices.contains(name.as_str()) {
                continue;
            }
            let Ok(old_device) = old_device.lock() else {
                continue;
            };
            if old_device.last.is_none() || old_device.last == Some(old_device.def_target) {
                continue;
            }
            let applied = self.apply_battery_scale(name, old_device.def_target);
            if old_device.last_applied == Some(applied) {
                continue;
            }
            if !dry_run {
                if let Some(path) = &old_device.path {
                    let payload = if let Some(write_name) = &old_device.write_name {
                        format!("{write_name} {applied}")
                    } else {
                        format!("{applied}")
                    };
                    let _ = fs::write(path, payload);
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn copy_state_from(&self, old: &DeviceStore) {
        for (name, old_device) in &old.devices {
            let Some(new_device) = self.devices.get(name) else {
                continue;
            };
            let Ok(old_device) = old_device.lock() else {
                continue;
            };
            if let Ok(mut new_device) = new_device.lock() {
                new_device.last = old_device.last;
                new_device.last_applied = old_device.last_applied;
            }
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.devices.contains_key(name)
    }

    pub fn default_target(&self, name: &str) -> i64 {
        self.devices
            .get(name)
            .and_then(|device| device.lock().ok().map(|device| device.def_target))
            .unwrap_or(0)
    }

    pub fn levels(&self, name: &str) -> Option<Arc<[i64]>> {
        self.devices
            .get(name)
            .and_then(|device| device.lock().ok().map(|device| Arc::clone(&device.levels)))
    }

    #[cfg(test)]
    pub fn set_target(&self, name: &str, target: i64, dry_run: bool) -> Result<i64, String> {
        self.set_target_for(0, name, target, dry_run)
    }

    pub fn set_target_for(
        &self,
        requester: u32,
        name: &str,
        target: i64,
        dry_run: bool,
    ) -> Result<i64, String> {
        let Some(device) = self.devices.get(name) else {
            return Err(format!("unknown device {name}"));
        };
        let mut device = device
            .lock()
            .map_err(|_| format!("device {name} poisoned"))?;
        device.requests.insert(requester, target);
        let mut chosen = device.def_target;
        for request in device.requests.values() {
            if device.select_higher {
                chosen = chosen.max(*request);
            } else {
                chosen = chosen.min(*request);
            }
        }
        if let Some(limit) = device.limit {
            if device.select_higher {
                chosen = chosen.min(limit);
            } else {
                chosen = chosen.max(limit);
            }
        }
        let effective = chosen;
        let applied = self.apply_battery_scale(name, effective);
        // Respect Kave Cosmic Charging Control Speed override while battery temp < 45.0C
        // (prevents both WiredTherma and SIC from fighting 18W/33W/MAX; safety guard resumes at >=45C).
        let is_charging_device = name == "battery"
            || name.contains("sic")
            || device.path.as_deref().is_some_and(|p| {
                let s = p.to_string_lossy();
                s.contains("constant_charge_current") || s.contains("charge_control_limit")
            });
        if !dry_run && is_charging_device && cosmic_charging_override_active() {
            return Ok(0);
        }
        if device.last == Some(effective) && device.last_applied == Some(applied) {
            return Ok(applied);
        }
        #[cfg(feature = "debug-log")]
        debug_log(&format!(
            "{name}: requested={target} effective={effective} applied={applied} previous={:?}",
            device.last
        ));
        if !dry_run {
            if let Some(path) = &device.path {
                let payload = if let Some(write_name) = &device.write_name {
                    format!("{write_name} {applied}")
                } else {
                    format!("{applied}")
                };
                if let Err(error) = fs::write(path, payload) {
                    #[cfg(feature = "debug-log")]
                    debug_log(&format!("{name}: write {} failed: {error}", path.display()));
                    return Err(format!("{}: {error}", path.display()));
                }
                #[cfg(feature = "debug-log")]
                debug_log(&format!("{name}: wrote {} value={applied}", path.display()));
            }
        }
        device.last = Some(effective);
        device.last_applied = Some(applied);
        Ok(applied)
    }

    #[cfg(test)]
    pub fn last(&self, name: &str) -> Option<i64> {
        self.devices
            .get(name)
            .and_then(|device| device.lock().ok().and_then(|device| device.last))
    }

    pub fn read_target(&self, name: &str) -> i64 {
        let path = self
            .devices
            .get(name)
            .and_then(|device| device.lock().ok().and_then(|device| device.path.clone()));
        path.and_then(|path| self.read_cached_last_i64(&path))
            .unwrap_or(0)
    }

    fn cached_file(&self, path: &Path) -> Option<CachedFile> {
        let mut cache = self.sysfs.lock().ok()?;
        if let Some(cell) = cache.get(path) {
            return Some(Arc::clone(cell));
        }
        let cell = Arc::new(Mutex::new(None));
        cache.insert(path.to_path_buf(), Arc::clone(&cell));
        Some(cell)
    }

    fn read_cached_i64(&self, path: &Path) -> Option<i64> {
        let cell = self.cached_file(path)?;
        let mut file = cell.lock().ok()?;
        if file.is_none() {
            *file = File::open(path).ok();
        }
        crate::sysfs::read_i64_file(file.as_mut()?)
    }

    fn read_cached_last_i64(&self, path: &Path) -> Option<i64> {
        let cell = self.cached_file(path)?;
        let mut file = cell.lock().ok()?;
        if file.is_none() {
            *file = File::open(path).ok();
        }
        crate::sysfs::read_last_i64_file(file.as_mut()?)
    }

    fn apply_battery_scale(&self, name: &str, target: i64) -> i64 {
        if target <= 99
            || !matches!(
                name,
                "battery" | "mag_wireless_charge" | "wireless_charge" | "wls_limit"
            )
        {
            return target;
        }
        let screen_state = self
            .read_sysfs_i64("sys/class/thermal/thermal_message/screen_state")
            .unwrap_or(-22);
        if screen_state == 0 {
            target % 100
        } else {
            target / 100
        }
    }

    pub fn read_sysfs_i64(&self, path: &str) -> Option<i64> {
        self.read_cached_i64(&self.root.join(path))
    }
}

#[cfg(feature = "debug-log")]
fn debug_log(text: &str) {
    use std::io::Write;

    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/data/local/tmp/hoshikv-thermal-debug.log")
    {
        let _ = writeln!(file, "{text}");
    }
}

fn resolve_device(
    root: &Path,
    config: &DeviceConfig,
    cooling: &HashMap<String, PathBuf>,
) -> (Option<PathBuf>, Option<String>) {
    if let Some(path) = &config.path {
        return (Some(resolve(root, path)), None);
    }
    if is_cpu_name(&config.name) {
        let limits = root.join("sys/class/thermal/thermal_message/cpu_limits");
        if limits.exists() {
            return (Some(limits), Some(config.name.clone()));
        }
        let path = root
            .join("sys/devices/system/cpu")
            .join(&config.name)
            .join("cpufreq/scaling_max_freq");
        if path.exists() {
            return (Some(path), None);
        }
    }
    if let Some(cooling_name) = &config.cooling_name {
        if let Some(path) = cooling.get(cooling_name) {
            return (Some(path.clone()), None);
        }
    }
    (cooling.get(&config.name).cloned(), None)
}

fn read_i64_path(path: &Path) -> Option<i64> {
    let mut file = File::open(path).ok()?;
    crate::sysfs::read_i64_file(&mut file)
}

fn read_cpu_levels(root: &Path, name: &str) -> Vec<i64> {
    let base = root
        .join("sys/devices/system/cpu")
        .join(name)
        .join("cpufreq");
    let mut levels = Vec::new();
    for file in ["scaling_available_frequencies", "scaling_boost_frequencies"] {
        let Ok(text) = fs::read_to_string(base.join(file)) else {
            continue;
        };
        levels.extend(
            text.split_whitespace()
                .filter_map(|value| value.parse::<i64>().ok()),
        );
    }
    levels.sort_unstable_by(|left, right| right.cmp(left));
    levels
}

const COSMIC_CHARGING_OVERRIDE: &str = "/data/vendor/kave_cosmic/charging_override";
const BATTERY_TEMP_PATH: &str = "/sys/class/power_supply/battery/temp";
const BATTERY_SAFETY_TEMP_DECIC: i32 = 450; // 45.0 C safety ceiling

fn cosmic_charging_override_active() -> bool {
    let enabled = fs::read_to_string(COSMIC_CHARGING_OVERRIDE)
        .ok()
        .map(|s| s.trim() == "1")
        .unwrap_or(false);
    if !enabled {
        return false;
    }
    let temp = fs::read_to_string(BATTERY_TEMP_PATH)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0);
    temp < BATTERY_SAFETY_TEMP_DECIC
}

pub fn is_cpu_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("cpu") else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|character| character.is_ascii_digit())
}

fn scan_cooling_devices(root: &Path) -> HashMap<String, PathBuf> {
    let mut devices = HashMap::new();
    let thermal = root.join("sys/class/thermal");
    let Ok(entries) = fs::read_dir(&thermal) else {
        return devices;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("cooling_device") {
            continue;
        }
        let dir = entry.path();
        let Ok(cooling_type) = fs::read_to_string(dir.join("type")) else {
            continue;
        };
        let cooling_type = cooling_type.trim();
        if cooling_type.is_empty() {
            continue;
        }
        devices
            .entry(cooling_type.to_owned())
            .or_insert_with(|| dir.join("cur_state"));
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_uses_cpu_limits_and_boost_levels() {
        let root = std::env::temp_dir().join(format!("mi_thermald_cpu_{}", std::process::id()));
        let limits = root.join("sys/class/thermal/thermal_message/cpu_limits");
        let cpufreq = root.join("sys/devices/system/cpu/cpu0/cpufreq");
        fs::create_dir_all(limits.parent().unwrap()).unwrap();
        fs::create_dir_all(&cpufreq).unwrap();
        fs::write(&limits, "").unwrap();
        fs::write(
            cpufreq.join("scaling_available_frequencies"),
            "2000000 1000000\n",
        )
        .unwrap();
        fs::write(cpufreq.join("scaling_boost_frequencies"), "3000000\n").unwrap();
        let mut store = DeviceStore::new(&root, &[]);
        store.ensure(&root, "cpu0");
        assert_eq!(
            store.levels("cpu0").as_deref(),
            Some(&[3000000, 2000000, 1000000][..])
        );
        assert_eq!(store.set_target("cpu0", 2000000, false).unwrap(), 2000000);
        assert_eq!(fs::read_to_string(&limits).unwrap(), "cpu0 2000000");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn aggregates_requests_like_device_selection() {
        let root =
            std::env::temp_dir().join(format!("mi_thermald_aggregate_{}", std::process::id()));
        let path = root.join("value");
        fs::create_dir_all(&root).unwrap();
        fs::write(&path, "0\n").unwrap();
        let store = DeviceStore::new(
            &root,
            &[DeviceConfig {
                name: "lower".to_owned(),
                cooling_name: None,
                path: Some(PathBuf::from("value")),
                def_target: 100,
                select_higher: false,
            }],
        );
        assert_eq!(store.set_target_for(1, "lower", 200, false).unwrap(), 100);
        assert_eq!(store.set_target_for(2, "lower", 50, false).unwrap(), 50);
        assert_eq!(fs::read_to_string(&path).unwrap(), "50");

        let higher = root.join("higher");
        fs::write(&higher, "0\n").unwrap();
        let store = DeviceStore::new(
            &root,
            &[DeviceConfig {
                name: "higher".to_owned(),
                cooling_name: None,
                path: Some(PathBuf::from("higher")),
                def_target: 100,
                select_higher: true,
            }],
        );
        assert_eq!(store.set_target_for(1, "higher", 200, false).unwrap(), 200);
        assert_eq!(store.set_target_for(2, "higher", 300, false).unwrap(), 300);
        assert_eq!(fs::read_to_string(&higher).unwrap(), "300");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scales_battery_targets_like_generic_setter() {
        let root = std::env::temp_dir().join(format!("mi_thermald_battery_{}", std::process::id()));
        let path = root.join("value");
        let screen = root.join("sys/class/thermal/thermal_message/screen_state");
        fs::create_dir_all(screen.parent().unwrap()).unwrap();
        fs::write(&path, "0").unwrap();
        fs::write(&screen, "1").unwrap();
        let store = DeviceStore::new(
            &root,
            &[DeviceConfig {
                name: "battery".to_owned(),
                cooling_name: None,
                path: Some(PathBuf::from("value")),
                def_target: 0,
                select_higher: true,
            }],
        );
        assert_eq!(store.set_target("battery", 900, false).unwrap(), 9);
        assert_eq!(fs::read_to_string(&path).unwrap(), "9");
        fs::write(&screen, "0").unwrap();
        assert_eq!(store.set_target("battery", 900, false).unwrap(), 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), "0");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn writes_target_once() {
        let root = std::env::temp_dir().join(format!("mi_thermald_device_{}", std::process::id()));
        let path = root.join("value");
        fs::create_dir_all(&root).unwrap();
        fs::write(&path, "0\n").unwrap();
        let store = DeviceStore::new(
            &root,
            &[DeviceConfig {
                name: "x".to_owned(),
                cooling_name: None,
                path: Some(PathBuf::from("value")),
                def_target: 0,
                select_higher: true,
            }],
        );
        assert_eq!(store.set_target("x", 42, false).unwrap(), 42);
        assert_eq!(fs::read_to_string(&path).unwrap(), "42");
        assert_eq!(store.last("x"), Some(42));
        assert_eq!(store.set_target("x", 7, false).unwrap(), 7);
        assert_eq!(fs::read_to_string(&path).unwrap(), "7");
        let _ = fs::remove_dir_all(&root);
    }
}

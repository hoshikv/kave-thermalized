use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::DeviceConfig;
use crate::thermal_sensors::resolve;

#[derive(Debug)]
pub struct Device {
    pub path: Option<PathBuf>,
    pub levels: Vec<i64>,
    pub def_target: i64,
    pub select_higher: bool,
    pub limit: Option<i64>,
    pub last: Option<i64>,
    write_name: Option<String>,
    requests: HashMap<String, i64>,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceStore {
    root: PathBuf,
    devices: HashMap<String, Arc<Mutex<Device>>>,
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
                levels,
                def_target: config.def_target,
                select_higher: config.select_higher,
                limit: None,
                last: None,
                write_name,
                requests: HashMap::new(),
            };
            devices.insert(config.name.clone(), Arc::new(Mutex::new(device)));
        }

        Self {
            root: root.to_path_buf(),
            devices,
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
        let levels = if cpu {
            read_cpu_levels(root, name)
        } else {
            Vec::new()
        };
        let def_target = if cpu {
            let current = read_i64_file(
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
            write_name,
            requests: HashMap::new(),
        };
        self.devices
            .insert(name.to_owned(), Arc::new(Mutex::new(device)));
    }

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

    pub fn levels(&self, name: &str) -> Vec<i64> {
        self.devices
            .get(name)
            .and_then(|device| device.lock().ok().map(|device| device.levels.clone()))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn set_target(&self, name: &str, target: i64, dry_run: bool) -> Result<i64, String> {
        self.set_target_for("__default__", name, target, dry_run)
    }

    pub fn set_target_for(
        &self,
        requester: &str,
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
        device.requests.insert(requester.to_owned(), target);
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
        let selected_nonzero = device.requests.values().any(|request| {
            *request != 0
                && if device.select_higher {
                    *request > device.def_target
                } else {
                    *request < device.def_target
                }
        });
        if device.last == Some(effective) && !selected_nonzero {
            return Ok(applied);
        }
        if !dry_run {
            if let Some(path) = &device.path {
                let mut options = fs::OpenOptions::new();
                options.write(true);
                if device.write_name.is_none() {
                    options.truncate(true);
                }
                let mut file = options
                    .open(path)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                if let Some(write_name) = &device.write_name {
                    write!(file, "{write_name} {applied}")
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                } else {
                    write!(file, "{applied}")
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                }
            }
        }
        device.last = Some(effective);
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
        path.and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| text.split_whitespace().last()?.parse().ok())
            .unwrap_or(0)
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
        fs::read_to_string(self.root.join(path))
            .ok()
            .and_then(|text| text.trim().parse().ok())
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

fn read_i64_file(path: &Path) -> Option<i64> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
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
        assert_eq!(store.levels("cpu0"), vec![3000000, 2000000, 1000000]);
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
        assert_eq!(store.set_target_for("a", "lower", 200, false).unwrap(), 100);
        assert_eq!(store.set_target_for("b", "lower", 50, false).unwrap(), 50);
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
        assert_eq!(
            store.set_target_for("a", "higher", 200, false).unwrap(),
            200
        );
        assert_eq!(
            store.set_target_for("b", "higher", 300, false).unwrap(),
            300
        );
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

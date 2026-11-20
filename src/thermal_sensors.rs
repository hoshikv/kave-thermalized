use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::ThermalConfig;

#[derive(Clone, Debug)]
pub enum SensorDef {
    Generic {
        path: PathBuf,
    },
    Simulated {
        path: PathBuf,
    },
    Virtual {
        members: Vec<(String, i64)>,
        weight_sum: i64,
        compensation: i64,
    },
}

#[derive(Clone, Copy, Debug, Default)]
struct SensorHistory {
    current: i64,
    last: i64,
    last2: i64,
}

#[derive(Clone, Debug)]
pub struct SensorStore {
    root: PathBuf,
    defs: HashMap<String, SensorDef>,
    history: Arc<Mutex<HashMap<String, SensorHistory>>>,
    virtual_lock: Arc<Mutex<()>>,
}

impl SensorStore {
    pub fn new(root: &Path, config: &ThermalConfig) -> Result<Self, String> {
        let mut defs = scan_thermal_zones(root)?;

        for section in &config.sections {
            match section.get("algo_type") {
                Some("simulated") => {
                    let Some(path) = section.get("path") else {
                        continue;
                    };
                    defs.insert(
                        section.name.clone(),
                        SensorDef::Simulated {
                            path: resolve(root, Path::new(path)),
                        },
                    );
                }
                Some("Virtual") => {
                    let members = section.words("sensors");
                    let weights = section.ints("weight");
                    let computed_sum: i64 = weights.iter().sum();
                    let weight_sum = section.int("weight_sum", computed_sum);
                    let compensation = section.int("compensation", 0);
                    let members = members
                        .into_iter()
                        .enumerate()
                        .map(|(index, name)| (name, weights.get(index).copied().unwrap_or(0)))
                        .collect();
                    defs.insert(
                        section.name.clone(),
                        SensorDef::Virtual {
                            members,
                            weight_sum,
                            compensation,
                        },
                    );
                }
                _ => {}
            }
        }

        Ok(Self {
            root: root.to_path_buf(),
            defs,
            history: Arc::new(Mutex::new(HashMap::new())),
            virtual_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn read(&self, name: &str) -> Result<i64, String> {
        self.read_inner(name, 0)
    }

    pub fn copy_state_from(&self, old: &SensorStore) {
        let Ok(old_history) = old.history.lock() else {
            return;
        };
        if let Ok(mut history) = self.history.lock() {
            for (name, value) in old_history.iter() {
                if self.defs.contains_key(name) {
                    history.insert(name.clone(), *value);
                }
            }
        }
    }

    pub fn history(&self, name: &str) -> (i64, i64) {
        self.history
            .lock()
            .ok()
            .and_then(|history| {
                history
                    .get(name)
                    .map(|history| (history.last, history.last2))
            })
            .unwrap_or((0, 0))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.defs.contains_key(name)
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.defs.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    fn read_inner(&self, name: &str, depth: usize) -> Result<i64, String> {
        if depth > 8 {
            return Err(format!("virtual sensor recursion at {name}"));
        }
        let Some(def) = self.defs.get(name) else {
            return Err(format!("unknown sensor {name}"));
        };
        match def {
            SensorDef::Generic { path } | SensorDef::Simulated { path } => read_i64(path),
            SensorDef::Virtual {
                members,
                weight_sum,
                compensation,
            } => {
                let _guard = self
                    .virtual_lock
                    .lock()
                    .map_err(|_| format!("virtual sensor lock for {name} poisoned"))?;
                let last = self
                    .history
                    .lock()
                    .ok()
                    .and_then(|history| history.get(name).map(|history| history.current))
                    .unwrap_or(0);
                let mut retries = 0;
                loop {
                    let mut sum = 0;
                    for (member, weight) in members {
                        let mut value = self.read_inner(member, depth + 1).unwrap_or(0);
                        if matches!(value, -40000 | 125000 | 0) {
                            value = self.read_inner(member, depth + 1).unwrap_or(0);
                        }
                        sum += value.saturating_mul(*weight);
                    }
                    let divisor = if *weight_sum == 0 { 1 } else { *weight_sum };
                    let board_comp = read_i64(
                        &self
                            .root
                            .join("sys/class/thermal/thermal_message/board_sensor_temp_comp"),
                    )
                    .unwrap_or(0);
                    let value = sum / divisor + compensation + board_comp;
                    let glitch = last != 0 && (value - last).abs() > 10000;
                    if retries >= 3 || !glitch {
                        return Ok(value);
                    }
                    retries += 1;
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }
}

pub fn resolve(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        root.join(path.strip_prefix("/").unwrap_or(path))
    } else {
        root.join(path)
    }
}

fn scan_thermal_zones(root: &Path) -> Result<HashMap<String, SensorDef>, String> {
    let thermal = root.join("sys/class/thermal");
    let mut defs = HashMap::new();
    let entries = match fs::read_dir(&thermal) {
        Ok(entries) => entries,
        Err(_) => return Ok(defs),
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let dir = entry.path();
        let Ok(sensor_type) = fs::read_to_string(dir.join("type")) else {
            continue;
        };
        let sensor_type = sensor_type.trim();
        if sensor_type.is_empty() {
            continue;
        }
        defs.insert(
            sensor_type.to_owned(),
            SensorDef::Generic {
                path: dir.join("temp"),
            },
        );
    }

    Ok(defs)
}

fn read_i64(path: &Path) -> Result<i64, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    text.trim()
        .parse::<i64>()
        .map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_thermal;

    #[test]
    fn virtual_sensor_uses_weights() {
        let root = std::env::temp_dir().join(format!("mi_thermald_sensor_{}", std::process::id()));
        let zone = root.join("sys/class/thermal/thermal_zone0");
        fs::create_dir_all(&zone).unwrap();
        fs::write(zone.join("type"), "a\n").unwrap();
        fs::write(zone.join("temp"), "10000\n").unwrap();
        let comp = root.join("sys/class/thermal/thermal_message");
        fs::create_dir_all(&comp).unwrap();
        fs::write(comp.join("board_sensor_temp_comp"), "5\n").unwrap();
        let config = parse_thermal(
            "[V]\nalgo_type Virtual\nsensors\ta\ta\nweight\t1 1\nweight_sum\t2\ncompensation\t5\n",
        );
        let store = SensorStore::new(&root, &config).unwrap();
        assert_eq!(store.read("V").unwrap(), 10010);
        let _ = fs::remove_dir_all(&root);
    }
}

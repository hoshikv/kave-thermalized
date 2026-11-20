use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::crypto;

#[derive(Clone, Debug)]
pub struct Section {
    pub name: String,
    pub values: HashMap<String, String>,
}

impl Section {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|s| s.as_str())
    }

    pub fn ints(&self, key: &str) -> Vec<i64> {
        self.get(key)
            .map(|value| {
                value
                    .split_whitespace()
                    .filter_map(|part| part.parse::<i64>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn int(&self, key: &str, default: i64) -> i64 {
        self.get(key)
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(default)
    }

    pub fn words(&self, key: &str) -> Vec<String> {
        self.get(key)
            .map(|value| value.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    pub fn plus_words(&self, key: &str) -> Vec<String> {
        self.get(key)
            .map(|value| {
                value
                    .split('+')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ThermalConfig {
    pub sections: Vec<Section>,
}

#[derive(Clone, Debug)]
pub struct DeviceConfig {
    pub name: String,
    pub cooling_name: Option<String>,
    pub path: Option<PathBuf>,
    pub def_target: i64,
    pub select_higher: bool,
}

pub fn parse_thermal(text: &str) -> ThermalConfig {
    let mut sections = Vec::new();
    let mut current: Option<Section> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            if let Some(end) = line.find(']') {
                let name = line[1..end].trim().to_owned();
                if let Some(section) = current.take() {
                    sections.push(section);
                }
                current = Some(Section {
                    name,
                    values: HashMap::new(),
                });
                continue;
            }
        }
        if let Some(section) = current.as_mut() {
            let split = line.find(char::is_whitespace).unwrap_or(line.len());
            let key = line[..split].trim().to_owned();
            let value = line[split..].trim().to_owned();
            if !key.is_empty() {
                section.values.insert(key, value);
            }
        }
    }

    if let Some(section) = current {
        sections.push(section);
    }

    ThermalConfig { sections }
}

pub fn parse_thermal_map(text: &str) -> Vec<(i64, String)> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(inner) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        else {
            continue;
        };
        let Some((index, file)) = inner.split_once(':') else {
            continue;
        };
        let Ok(index) = index.trim().parse::<i64>() else {
            continue;
        };
        let file = file.trim();
        if (0..0x801).contains(&index) && !file.is_empty() {
            entries.push((index, file.to_owned()));
        }
    }
    entries
}

pub fn parse_region_map(text: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((version, file)) = line.split_once(':') else {
            continue;
        };
        let version = version.trim();
        let file = file.trim();
        if !version.is_empty() && !file.is_empty() {
            entries.push((version.to_owned(), file.to_owned()));
        }
    }
    entries
}

pub fn parse_devices(text: &str) -> Vec<DeviceConfig> {
    let mut devices = Vec::new();
    let mut current: Option<DeviceConfig> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line == "[" {
            if let Some(device) = current.take() {
                devices.push(device);
            }
            current = Some(DeviceConfig {
                name: String::new(),
                cooling_name: None,
                path: None,
                def_target: 0,
                select_higher: true,
            });
            continue;
        }
        if line == "]" {
            if let Some(device) = current.take() {
                devices.push(device);
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Some(device) = current.as_mut() else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "name" => device.name = value.to_owned(),
            "cooling_name" => device.cooling_name = Some(value.to_owned()),
            "path" => device.path = Some(PathBuf::from(value)),
            "def_target" => device.def_target = value.parse().unwrap_or(0),
            "select_higher" => device.select_higher = value.parse::<i64>().unwrap_or(1) != 0,
            _ => {}
        }
    }

    if let Some(device) = current {
        devices.push(device);
    }

    devices.retain(|device| !device.name.is_empty());
    devices
}

pub fn load_text(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let plain = is_plain_text(&bytes);
    let bytes = if plain {
        bytes
    } else {
        crypto::decrypt(&bytes)?
    };

    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    let text = String::from_utf8(bytes[..end].to_vec())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(text)
}

fn is_plain_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    text.chars().all(|character| {
        character == '\n' || character == '\r' || character == '\t' || !character.is_control()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_and_values() {
        let text = "[A]\nalgo_type\tss\nsensor\tS0\ndevice\tcpu0+cpu1\ntrig\t1 2 3\ntarget\t10+11 20+21 30+31\n";
        let config = parse_thermal(text);
        assert_eq!(config.sections.len(), 1);
        let section = &config.sections[0];
        assert_eq!(section.name, "A");
        assert_eq!(section.get("algo_type"), Some("ss"));
        assert_eq!(section.ints("trig"), vec![1, 2, 3]);
        assert_eq!(section.plus_words("device"), vec!["cpu0", "cpu1"]);
    }

    #[test]
    fn parses_region_map() {
        let entries = parse_region_map("8.21.0:thermal-map-india.conf\n# x\n8.22.0:other.conf\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "8.21.0");
        assert_eq!(entries[0].1, "thermal-map-india.conf");
    }

    #[test]
    fn parses_thermal_map() {
        let text = "[0:thermal-normal.conf]\n[27:thermal-charge.conf]\n[bad]\n";
        assert_eq!(
            parse_thermal_map(text),
            vec![
                (0, "thermal-normal.conf".to_owned()),
                (27, "thermal-charge.conf".to_owned())
            ]
        );
    }

    #[test]
    fn parses_device_blocks() {
        let text = "# x\n[\nname:gpu\ncooling_name:gpu\ndef_target:0\nselect_higher:1\n]\n";
        let devices = parse_devices(text);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "gpu");
        assert_eq!(devices[0].cooling_name.as_deref(), Some("gpu"));
        assert!(devices[0].select_higher);
    }
}

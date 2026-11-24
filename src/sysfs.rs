use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const RAW_CACHE_TTL: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, Debug)]
struct CachedValue {
    value: i64,
    at: Instant,
}

#[derive(Debug, Default)]
pub struct RawCache {
    entries: Mutex<HashMap<PathBuf, CachedValue>>,
}

impl RawCache {
    pub(crate) fn get(&self, path: &Path) -> Option<i64> {
        let entries = self.entries.lock().ok()?;
        let entry = entries.get(path)?;
        (entry.at.elapsed() < RAW_CACHE_TTL).then_some(entry.value)
    }

    pub(crate) fn put(&self, path: &Path, value: i64) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                path.to_path_buf(),
                CachedValue {
                    value,
                    at: Instant::now(),
                },
            );
        }
    }

    pub fn invalidate(&self, path: &Path) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(path);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
    }
}

pub struct SysfsNode {
    file: File,
    path: PathBuf,
}

impl SysfsNode {
    pub fn open(path: &str) -> Option<Self> {
        File::open(path).ok().map(|file| Self {
            file,
            path: PathBuf::from(path),
        })
    }

    pub fn read_i32(&mut self) -> Option<i32> {
        read_i64_file(&mut self.file).and_then(|value| i32::try_from(value).ok())
    }

    pub fn read_i32_cached(&mut self, cache: Option<&RawCache>) -> Option<i32> {
        if let Some(value) = cache.and_then(|cache| cache.get(&self.path)) {
            return i32::try_from(value).ok();
        }
        let value = self.read_i32()?;
        if let Some(cache) = cache {
            cache.put(&self.path, i64::from(value));
        }
        Some(value)
    }
}

pub fn read_i64_file(file: &mut File) -> Option<i64> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut buf = [0u8; 64];
    let len = file.read(&mut buf).ok()?;
    std::str::from_utf8(&buf[..len]).ok()?.trim().parse().ok()
}

pub fn read_last_i64_file(file: &mut File) -> Option<i64> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut buf = [0u8; 256];
    let len = file.read(&mut buf).ok()?;
    std::str::from_utf8(&buf[..len])
        .ok()?
        .split_whitespace()
        .last()?
        .parse()
        .ok()
}

pub fn find_zone_by_type(zone_type: &str) -> Option<String> {
    for idx in 0..128 {
        let type_path = format!("/sys/class/thermal/thermal_zone{}/type", idx);
        if !Path::new(&type_path).exists() {
            break;
        }
        if let Ok(t) = std::fs::read_to_string(&type_path) {
            if t.trim() == zone_type {
                return Some(format!("/sys/class/thermal/thermal_zone{}/temp", idx));
            }
        }
    }
    None
}

pub fn find_cdev_by_type(cdev_type: &str) -> Option<String> {
    for idx in 0..128 {
        let type_path = format!("/sys/class/thermal/cooling_device{}/type", idx);
        if !Path::new(&type_path).exists() {
            break;
        }
        if let Ok(t) = std::fs::read_to_string(&type_path) {
            if t.trim() == cdev_type {
                return Some(format!(
                    "/sys/class/thermal/cooling_device{}/cur_state",
                    idx
                ));
            }
        }
    }
    None
}

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub struct SysfsNode {
    file: File,
}

impl SysfsNode {
    pub fn open(path: &str) -> Option<Self> {
        File::open(path).ok().map(|file| Self { file })
    }

    pub fn read_i32(&mut self) -> Option<i32> {
        self.file.seek(SeekFrom::Start(0)).ok()?;
        let mut buf = String::with_capacity(32);
        self.file.read_to_string(&mut buf).ok()?;
        buf.trim().parse().ok()
    }
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

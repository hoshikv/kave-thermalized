use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub struct SysfsNode {
    file: File,
    path: String,
    writable: bool,
    fail_logged: bool,
}

impl SysfsNode {
    pub fn open(path: &str, writable: bool) -> Option<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)
            .ok()?;
        Some(Self {
            file,
            path: path.to_string(),
            writable,
            fail_logged: false,
        })
    }

    pub fn read_i32(&mut self) -> Option<i32> {
        self.file.seek(SeekFrom::Start(0)).ok()?;
        let mut buf = String::with_capacity(32);
        self.file.read_to_string(&mut buf).ok()?;
        buf.trim().parse().ok()
    }

    pub fn read_string(&mut self) -> Option<String> {
        self.file.seek(SeekFrom::Start(0)).ok()?;
        let mut buf = String::with_capacity(64);
        self.file.read_to_string(&mut buf).ok()?;
        Some(buf.trim().to_string())
    }

    pub fn write_i32(&mut self, value: i32) -> bool {
        if !self.writable {
            return false;
        }
        self.write_raw(&format!("{}", value))
    }

    pub fn write_raw(&mut self, content: &str) -> bool {
        if !self.writable {
            crate::klog(&format!("WRITE_SKIP(not-writable) {} <- {:?}", self.path, content.trim()));
            return false;
        }
        if let Err(e) = self.file.seek(SeekFrom::Start(0)) {
            crate::klog(&format!("WRITE_FAIL(seek) {} <- {:?} err={}", self.path, content.trim(), e));
            return false;
        }
        if let Err(e) = self.file.set_len(0) {
            crate::klog(&format!("WRITE_FAIL(truncate) {} <- {:?} err={}", self.path, content.trim(), e));
            return false;
        }
        match self.file.write_all(content.as_bytes()) {
            Ok(_) => {
                crate::klog(&format!("WRITE_OK {} <- {:?}", self.path, content.trim()));
                self.fail_logged = false;
                true
            }
            Err(e) => {
                crate::klog(&format!("WRITE_FAIL(io) {} <- {:?} err={}", self.path, content.trim(), e));
                self.fail_logged = true;
                false
            }
        }
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

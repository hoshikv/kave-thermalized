use crate::mi_thermald::abx_setting_value;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const SF_SERVICE: &str = "SurfaceFlinger";
const SF_SET_ACTIVE_CONFIG: u32 = 1035;

/// SurfaceFlinger hardware mode IDs on peridot (all group=0, seamless switch):
/// id=0 -> 60 Hz, id=1 -> 120 Hz, -1 -> release override back to DisplayManager policy.
const MODE_IDLE_60HZ: i32 = 0;
const MODE_ACTIVE_120HZ: i32 = 1;
const MODE_RELEASE_OVERRIDE: i32 = -1;

const DEFAULT_IDLE_TIMEOUT_MS: u64 = 3000;
const GPU_BUSY_THRESHOLD_PCT: u64 = 25;
const SCREEN_STATE_PATH: &str = "/sys/class/thermal/thermal_message/screen_state";
const GPU_BUSY_PATH: &str = "/sys/class/kgsl/kgsl-3d0/gpubusy";
const INPUT_DEVICES_PATH: &str = "/proc/bus/input/devices";

pub fn spawn(shutdown: &'static AtomicBool) {
    if matches!(
        std::env::var("HOSHIKV_IDLE_FPS").ok().as_deref(),
        Some("0" | "false" | "no" | "off")
    ) {
        crate::klog("idle-fps disabled via HOSHIKV_IDLE_FPS");
        return;
    }

    let timeout_ms = std::env::var("HOSHIKV_IDLE_FPS_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&ms| ms >= 500)
        .unwrap_or(DEFAULT_IDLE_TIMEOUT_MS);

    let _ = thread::Builder::new()
        .name("idle-fps".into())
        .spawn(move || run(shutdown, Duration::from_millis(timeout_ms)));
}

fn run(shutdown: &AtomicBool, idle_timeout: Duration) {
    // Wait briefly so binder_thread initializes ProcessState first.
    thread::sleep(Duration::from_secs(3));

    let Some(touch_path) = wait_for_touch_device(shutdown) else {
        return;
    };

    let Ok(mut touch_file) = File::open(&touch_path) else {
        crate::klog(&format!("idle-fps: failed to open {}", touch_path.display()));
        return;
    };

    // Set non-blocking so we can drain queued input_event bursts cleanly after poll().
    let fd = touch_file.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }

    crate::klog(&format!(
        "idle-fps active: touch={} timeout={}ms",
        touch_path.display(),
        idle_timeout.as_millis()
    ));

    let mut sf = SurfaceFlingerClient::default();
    let mut settings = DisplaySettingsCache::new();
    let mut in_idle_60 = false;
    let mut needs_restore_120 = false;
    let mut was_screen_on = is_screen_on();
    let mut last_touch = Instant::now();
    let mut last_state_check = Instant::now() - Duration::from_secs(10);
    let mut drain_buf = [0u8; 512];

    while !shutdown.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now.duration_since(last_state_check) >= Duration::from_secs(2) {
            last_state_check = now;
            let screen_on = is_screen_on();
            if !screen_on {
                if in_idle_60 && sf.set_mode(MODE_RELEASE_OVERRIDE) {
                    in_idle_60 = false;
                    // ColorOS SF stays at 60Hz after -1 until mode 1 is sent when screen is ON.
                    needs_restore_120 = true;
                }
                was_screen_on = false;
                thread::sleep(Duration::from_secs(2));
                continue;
            }

            // Screen just transitioned OFF -> ON: drain stale touch events, let HWC settle briefly,
            // and restore 120Hz if we had dropped to 60Hz before screen-off.
            if !was_screen_on {
                was_screen_on = true;
                in_idle_60 = false;
                drain_touch(&mut touch_file, &mut drain_buf);
                if needs_restore_120 && settings.should_idle_drop() {
                    thread::sleep(Duration::from_millis(250));
                    if sf.set_mode(MODE_ACTIVE_120HZ) {
                        let _ = sf.set_mode(MODE_RELEASE_OVERRIDE);
                        needs_restore_120 = false;
                    }
                } else {
                    needs_restore_120 = false;
                }
                last_touch = Instant::now();
            }

            if !settings.should_idle_drop() {
                if in_idle_60 || needs_restore_120 {
                    let _ = sf.set_mode(MODE_ACTIVE_120HZ);
                    if sf.set_mode(MODE_RELEASE_OVERRIDE) {
                        in_idle_60 = false;
                        needs_restore_120 = false;
                    }
                }
                thread::sleep(Duration::from_secs(2));
                last_touch = Instant::now();
                continue;
            }
        }

        if in_idle_60 {
            // At 60Hz: block in kernel poll() for instant (<1ms) wake on first touch,
            // waking at most once every 2s to re-check screen_state/settings.
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, 2000) };
            if rc > 0 && (pfd.revents & libc::POLLIN) != 0 {
                drain_touch(&mut touch_file, &mut drain_buf);
                last_touch = Instant::now();
                if sf.set_mode(MODE_ACTIVE_120HZ) {
                    let _ = sf.set_mode(MODE_RELEASE_OVERRIDE);
                    in_idle_60 = false;
                }
            }
        } else {
            // At 120Hz: never wake at 240Hz during scrolling — sleep 500ms, then drain any
            // accumulated touch events in a single non-blocking read.
            thread::sleep(Duration::from_millis(500));
            if drain_touch(&mut touch_file, &mut drain_buf) {
                last_touch = Instant::now();
            } else if last_touch.elapsed() >= idle_timeout && !is_gpu_busy() {
                if sf.set_mode(MODE_IDLE_60HZ) {
                    in_idle_60 = true;
                }
            }
        }
    }

    if in_idle_60 {
        let _ = sf.set_mode(MODE_ACTIVE_120HZ);
        let _ = sf.set_mode(MODE_RELEASE_OVERRIDE);
    }
}

fn drain_touch(file: &mut File, buf: &mut [u8]) -> bool {
    let mut saw_event = false;
    while let Ok(n) = file.read(buf) {
        if n == 0 {
            break;
        }
        saw_event = true;
    }
    saw_event
}

fn wait_for_touch_device(shutdown: &AtomicBool) -> Option<PathBuf> {
    for attempt in 0..30 {
        if shutdown.load(Ordering::Relaxed) {
            return None;
        }
        if let Ok(text) = fs::read_to_string(INPUT_DEVICES_PATH) {
            if let Some(event) = find_touch_event(&text) {
                let path = PathBuf::from("/dev/input").join(event);
                if path.exists() {
                    return Some(path);
                }
            }
        }
        if attempt == 0 {
            crate::klog("idle-fps: waiting for touchscreen input device");
        }
        thread::sleep(Duration::from_secs(2));
    }
    crate::klog("idle-fps: touchscreen input device not found, stopping");
    None
}

fn find_touch_event(proc_devices: &str) -> Option<String> {
    for block in proc_devices.split("\n\n") {
        let mut is_direct_prop = false;
        let mut is_touch_name = false;
        let mut event_name: Option<String> = None;

        for line in block.lines() {
            if let Some(name) = line.strip_prefix("N: Name=") {
                let lower = name.to_ascii_lowercase();
                if lower.contains("goodix")
                    || lower.contains("fts")
                    || lower.contains("focaltech")
                    || lower.contains("touchscreen")
                    || lower.contains("nt36")
                    || lower.contains("sec_ts")
                {
                    is_touch_name = true;
                }
            } else if let Some(prop) = line.strip_prefix("B: PROP=") {
                if let Ok(mask) = u64::from_str_radix(prop.trim(), 16) {
                    // INPUT_PROP_DIRECT = 0x01 -> bit 1 (0x2) in PROP bitmap
                    if (mask & 0x2) != 0 {
                        is_direct_prop = true;
                    }
                }
            } else if let Some(handlers) = line.strip_prefix("H: Handlers=") {
                for token in handlers.split_whitespace() {
                    if token.starts_with("event") {
                        event_name = Some(token.to_owned());
                    }
                }
            }
        }

        if (is_direct_prop || is_touch_name) && event_name.is_some() {
            return event_name;
        }
    }
    None
}

fn is_screen_on() -> bool {
    match fs::read_to_string(SCREEN_STATE_PATH) {
        Ok(val) => val.trim() == "1",
        Err(_) => true,
    }
}

fn is_gpu_busy() -> bool {
    fs::read_to_string(GPU_BUSY_PATH)
        .ok()
        .and_then(|s| parse_gpu_busy_pct(&s))
        .map(|pct| pct >= GPU_BUSY_THRESHOLD_PCT)
        .unwrap_or(false)
}

fn parse_gpu_busy_pct(raw: &str) -> Option<u64> {
    let mut parts = raw.split_whitespace();
    let busy: u64 = parts.next()?.parse().ok()?;
    let total: u64 = parts.next()?.parse().ok()?;
    if total == 0 {
        return Some(0);
    }
    Some(busy.saturating_mul(100) / total)
}

#[derive(Default)]
struct SurfaceFlingerClient {
    binder: Option<rsbinder::SIBinder>,
}

impl SurfaceFlingerClient {
    fn set_mode(&mut self, mode: i32) -> bool {
        if self.binder.is_none() {
            self.binder = rsbinder::hub::check_service(SF_SERVICE);
        }
        let Some(binder) = self.binder.as_ref() else {
            return false;
        };
        let Some(remote) = binder.as_remote() else {
            self.binder = None;
            return false;
        };
        let Ok(mut data) = remote.prepare_transact(true) else {
            self.binder = None;
            return false;
        };
        if data.write(&mode).is_err() {
            return false;
        }
        match remote.submit_transact(SF_SET_ACTIVE_CONFIG, &data, 0) {
            Ok(_) => true,
            Err(_) => {
                // SurfaceFlinger may have restarted; drop cached binder so next call reconnects.
                self.binder = None;
                false
            }
        }
    }
}

struct DisplaySettingsCache {
    system_xml: PathBuf,
    secure_xml: PathBuf,
    last_system_mtime: Option<SystemTime>,
    last_secure_mtime: Option<SystemTime>,
    high_perf: bool,
    standard_60_only: bool,
}

impl DisplaySettingsCache {
    fn new() -> Self {
        Self {
            system_xml: Path::new("/data/system/users/0/settings_system.xml").to_path_buf(),
            secure_xml: Path::new("/data/system/users/0/settings_secure.xml").to_path_buf(),
            last_system_mtime: None,
            last_secure_mtime: None,
            high_perf: false,
            standard_60_only: false,
        }
    }

    fn should_idle_drop(&mut self) -> bool {
        if let Ok(mtime) = fs::metadata(&self.system_xml).and_then(|m| m.modified()) {
            if self.last_system_mtime != Some(mtime) {
                self.last_system_mtime = Some(mtime);
                if let Ok(bytes) = fs::read(&self.system_xml) {
                    let hp =
                        abx_setting_value(&bytes, "high_performance_mode_on").as_deref() == Some("1");
                    let gt =
                        abx_setting_value(&bytes, "gt_mode_state_setting").as_deref() == Some("1");
                    self.high_perf = hp || gt;
                }
            }
        }

        if let Ok(mtime) = fs::metadata(&self.secure_xml).and_then(|m| m.modified()) {
            if self.last_secure_mtime != Some(mtime) {
                self.last_secure_mtime = Some(mtime);
                if let Ok(bytes) = fs::read(&self.secure_xml) {
                    // ColorOS: 2 = Standard 60Hz (no need to override)
                    self.standard_60_only =
                        abx_setting_value(&bytes, "oplus_customize_screen_refresh_rate").as_deref()
                            == Some("2");
                }
            }
        }

        !self.high_perf && !self.standard_60_only
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_goodix_touch_event() {
        let sample = "\
I: Bus=0019 Vendor=0001 Product=0001 Version=0100
N: Name=\"gpio-keys\"
H: Handlers=event0 cpufreq xiaomitouch
B: PROP=0
B: EV=3

I: Bus=0018 Vendor=0000 Product=0000 Version=0000
N: Name=\"goodix_ts\"
H: Handlers=event7 cpufreq xiaomitouch kgsl sde
B: PROP=2
B: EV=b
";
        assert_eq!(find_touch_event(sample).as_deref(), Some("event7"));
    }

    #[test]
    fn parses_gpu_busy_percentage() {
        assert_eq!(parse_gpu_busy_pct(" 302647 1004619\n"), Some(30));
        assert_eq!(parse_gpu_busy_pct(" 12000 1000000\n"), Some(1));
        assert_eq!(parse_gpu_busy_pct(" 0 0\n"), Some(0));
    }
}

mod config;
mod guard;
mod hal;
mod loadstat;
mod policy;
mod sensors;
mod sysfs;
mod tuner;

use guard::{ThermalGuard, ThermalRisk};
use libc::{SIGINT, SIGTERM};
use loadstat::LoadTracker;
use policy::StepPolicy;
use sensors::{SensorSet, Weights};
use std::io::Write as IoWrite;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysfs::{find_cdev_by_type, SysfsNode};
use tuner::Tuner;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

const TICK_MS: u64 = 1000;
const STATUS_INTERVAL_TICKS: u32 = 2;
const DUMP_PATH: &str = "/data/vendor/thermal/custom.dump";
const DUMP_MAX_LINES: usize = 128;

const JUNCTION_OFFSET: i32 = 8000;

#[allow(dead_code)]
struct Controls {
    cpu_limits: Option<SysfsNode>,
    temp_state: Option<SysfsNode>,
    board_temp: Option<SysfsNode>,
    board_other_temp: Option<SysfsNode>,
    torch_level: Option<SysfsNode>,
    voice_limit: Option<SysfsNode>,
    wifi_limit: Option<SysfsNode>,
    max_brightness: Option<SysfsNode>,
    modem_limit: Option<SysfsNode>,
    lmh: Option<SysfsNode>,
    gpu_cdev: Option<SysfsNode>,
    cdsp: Option<SysfsNode>,
    battery_cdev: Option<SysfsNode>,
    pause_cpu3: Option<SysfsNode>,
    pause_cpu4: Option<SysfsNode>,
    pause_cpu7: Option<SysfsNode>,
    thermal_board_temp: Option<SysfsNode>,
    fcc: Option<SysfsNode>,
    max_fcc: i32,
    usb_input_limit: Option<SysfsNode>,
    usb_online: Option<SysfsNode>,
    gpu_max_freq: Option<SysfsNode>,
    gpu_available_freqs: Option<SysfsNode>,
}

impl Controls {
    fn probe() -> Self {
        let msg = |name: &str| -> Option<SysfsNode> {
            SysfsNode::open(&format!("/sys/class/thermal/thermal_message/{}", name), true)
        };
        let cdev = |name: &str| -> Option<SysfsNode> {
            find_cdev_by_type(name).and_then(|p| SysfsNode::open(&p, true))
        };
        let qbat = |name: &str| -> Option<SysfsNode> {
            SysfsNode::open(&format!("/sys/class/qcom-battery/{}", name), true)
        };
        Self {
            cpu_limits: msg("cpu_limits"),
            temp_state: msg("temp_state"),
            board_temp: msg("board_sensor_temp"),
            board_other_temp: msg("board_sensor_other_temp"),
            torch_level: msg("torch_level"),
            voice_limit: msg("voice_limit"),
            wifi_limit: msg("wifi_limit"),
            max_brightness: msg("thermal_max_brightness"),
            modem_limit: msg("modem_limit"),
            lmh: cdev("cpu-cluster2"),
            gpu_cdev: cdev("gpu"),
            cdsp: cdev("cdsp"),
            battery_cdev: cdev("battery"),
            pause_cpu3: cdev("pause-cpu3"),
            pause_cpu4: cdev("pause-cpu4"),
            pause_cpu7: cdev("pause-cpu7"),
            thermal_board_temp: qbat("thermal_board_temp"),
            fcc: SysfsNode::open("/sys/class/power_supply/battery/constant_charge_current", true),
            max_fcc: {
                let v = std::fs::read_to_string("/sys/class/power_supply/battery/constant_charge_current")
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .unwrap_or(4250000);
                v
            },
            usb_input_limit: SysfsNode::open(
                "/sys/class/power_supply/usb/input_current_limit",
                false,
            ),
            usb_online: SysfsNode::open("/sys/class/power_supply/usb/online", false),
            gpu_max_freq: SysfsNode::open(
                "/sys/class/kgsl/kgsl-3d0/devfreq/max_freq",
                true,
            ),
            gpu_available_freqs: SysfsNode::open(
                "/sys/class/kgsl/kgsl-3d0/devfreq/available_frequencies",
                false,
            ),
        }
    }

    fn release_all(&mut self, base: &Baselines) {
        if let Some(n) = self.cpu_limits.as_mut() {
            n.write_raw(&format!("cpu0 {}\n", base.cpu0_max));
            n.write_raw(&format!("cpu3 {}\n", base.cpu3_max));
            n.write_raw(&format!("cpu7 {}\n", base.cpu7_max));
        }
        if let Some(n) = self.gpu_max_freq.as_mut() {
            n.write_i32(base.gpu_max);
        }
        if let Some(n) = self.gpu_cdev.as_mut() {
            n.write_i32(0);
        }
        if let Some(n) = self.cdsp.as_mut() {
            n.write_i32(0);
        }
        if let Some(n) = self.lmh.as_mut() {
            n.write_i32(0);
        }
        if let Some(n) = self.pause_cpu3.as_mut() {
            n.write_i32(0);
        }
        if let Some(n) = self.pause_cpu4.as_mut() {
            n.write_i32(0);
        }
        if let Some(n) = self.pause_cpu7.as_mut() {
            n.write_i32(0);
        }
        if let Some(n) = self.temp_state.as_mut() {
            n.write_i32(0);
        }
        if let Some(n) = self.voice_limit.as_mut() {
            n.write_i32(0);
        }
        if let Some(n) = self.wifi_limit.as_mut() {
            n.write_i32(0);
        }
        if let Some(n) = self.max_brightness.as_mut() {
            n.write_i32(255);
        }
        if let Some(n) = self.modem_limit.as_mut() {
            n.write_i32(0);
        }
        if let Some(n) = self.fcc.as_mut() {
            n.write_i32(self.max_fcc);
        }
    }
}

struct Baselines {
    cpu0_max: i32,
    cpu3_max: i32,
    cpu7_max: i32,
    gpu_max: i32,
}

fn read_cpuinfo_max(policy: usize) -> Option<i32> {
    let path = format!(
        "/sys/devices/system/cpu/cpufreq/policy{}/cpuinfo_max_freq",
        policy
    );
    let mut n = SysfsNode::open(&path, false)?;
    n.read_i32()
}

fn read_gpu_max_freq() -> Option<i32> {
    let mut n = SysfsNode::open("/sys/class/kgsl/kgsl-3d0/max_gpuclk", false)?;
    n.read_i32()
}

fn probe_baselines() -> Baselines {
    Baselines {
        cpu0_max: read_cpuinfo_max(0).unwrap_or(2016000),
        cpu3_max: read_cpuinfo_max(3).unwrap_or(2803200),
        cpu7_max: read_cpuinfo_max(7).unwrap_or(2918400),
        gpu_max: read_gpu_max_freq().unwrap_or(1100000000),
    }
}

#[allow(dead_code)]
struct ThermalEngine {
    cpu0: StepPolicy,
    cpu3: StepPolicy,
    cpu7: StepPolicy,
    gpu: StepPolicy,
    cdsp: StepPolicy,
    lmh: StepPolicy,
    hotplug: StepPolicy,
    tstate: StepPolicy,
    torch: StepPolicy,
    wifi: StepPolicy,
    brightness: StepPolicy,
    modem: StepPolicy,
    chg_bat: StepPolicy,
    chg_skin: StepPolicy,
    restrict_level: usize,
    suspend_level: usize,
    clamp_temps: Vec<i32>,
    clamp_currents: Vec<i32>,
    max_fcc: i32,
    voice_on: bool,
    wifi_on: bool,
    brightness_on: bool,
    modem_on: bool,
    chg_level: usize,
    suspend_on: bool,
    last_ccc: i32,
    framework_ccc: i32,
    last_board_written: i32,
    last_bright_written: i32,
    gpu_freq_table: Vec<i32>,
    last_batt_temp: i32,
    chg_rate_ema: f32,
    chg_boost_floor: i32,
}

impl ThermalEngine {
    fn new(framework_ccc: i32, cfg: &config::ThermalConfig, _base: &Baselines) -> Self {
        fn make_trig_clr_tgt<'a>(
            cfg_trig: Option<&'a [i32]>, cfg_clr: Option<&'a [i32]>, cfg_tgt: Option<&'a [i32]>,
            def_trig: &'a [i32], def_clr: &'a [i32], def_tgt: &'a [i32],
        ) -> (&'a [i32], &'a [i32], &'a [i32]) {
            (
                cfg_trig.unwrap_or(def_trig),
                cfg_clr.unwrap_or(def_clr),
                cfg_tgt.unwrap_or(def_tgt),
            )
        }

        let (t, c, g) = make_trig_clr_tgt(
            cfg.cpu0.as_ref().and_then(|x| x.trig.as_deref()),
            cfg.cpu0.as_ref().and_then(|x| x.clr.as_deref()),
            cfg.cpu0.as_ref().and_then(|x| x.targets.as_deref()),
            &[62000, 70000, 78000, 86000, 94000],
            &[56000, 64000, 72000, 80000, 88000],
            &[1478400, 1228800, 1017600, 902400, 787200],
        );
        let cpu0 = StepPolicy::new("CPU0", t, c, g);

        let (t, c, g) = make_trig_clr_tgt(
            cfg.cpu3.as_ref().and_then(|x| x.trig.as_deref()),
            cfg.cpu3.as_ref().and_then(|x| x.clr.as_deref()),
            cfg.cpu3.as_ref().and_then(|x| x.targets.as_deref()),
            &[62000, 70000, 78000, 86000, 94000, 99000, 103000],
            &[56000, 64000, 72000, 80000, 88000, 93000, 97000],
            &[1708800, 1497600, 1286400, 1056000, 940800, 787200, 633600],
        );
        let cpu3 = StepPolicy::new("CPU3", t, c, g);

        let (t, c, g) = make_trig_clr_tgt(
            cfg.cpu7.as_ref().and_then(|x| x.trig.as_deref()),
            cfg.cpu7.as_ref().and_then(|x| x.clr.as_deref()),
            cfg.cpu7.as_ref().and_then(|x| x.targets.as_deref()),
            &[62000, 70000, 78000, 86000, 94000, 99000, 103000],
            &[56000, 64000, 72000, 80000, 88000, 93000, 97000],
            &[1651200, 1459200, 1209600, 960000, 787200, 633600, 480000],
        );
        let cpu7 = StepPolicy::new("CPU7", t, c, g);

        let gpu_freqs_default: &[i32] = &[633000000, 500000000, 353000000, 255000000, 180000000, 120000000];
        let (t, c, g) = make_trig_clr_tgt(
            cfg.gpu.as_ref().and_then(|x| x.trig.as_deref()),
            cfg.gpu.as_ref().and_then(|x| x.clr.as_deref()),
            cfg.gpu.as_ref().and_then(|x| x.freqs.as_deref()),
            &[68000, 75000, 82000, 89000, 96000, 102000],
            &[62000, 69000, 76000, 83000, 90000, 96000],
            gpu_freqs_default,
        );
        let gpu = StepPolicy::new("GPU", t, c, g);

        let cdsp = StepPolicy::new("CDSP", &[60000, 70000, 80000, 90000], &[54000, 64000, 74000, 84000], &[3, 4, 5, 6]);

        Self {
            cpu0,
            cpu3,
            cpu7,
            gpu,
            cdsp,
            lmh: StepPolicy::new("LMH", &[95000], &[90000], &[2]),
            hotplug: StepPolicy::new("HOTPLUG", &[95000], &[90000], &[1]),
            tstate: StepPolicy::new("TSTATE", &[60000, 70000, 80000, 90000], &[54000, 64000, 74000, 84000], &[1, 2, 3, 4]),
            torch: StepPolicy::new("TORCH", &[52000, 57000], &[49000, 54000], &[1, 2]),
            wifi: StepPolicy::new("WIFI", &[50000, 55000, 60000], &[47000, 52000, 57000], &[1, 1, 1]),
            brightness: StepPolicy::new("BRIGHT", &[50000, 55000], &[47000, 52000], &[160, 100]),
            modem: StepPolicy::new("MODEM", &[60000, 70000], &[57000, 67000], &[1, 1]),
            chg_bat: {
                let t = cfg
                    .battery
                    .as_ref()
                    .and_then(|b| b.limit_temps.as_deref())
                    .unwrap_or(&[44000, 46000, 48000, 50000, 52000, 54000]);
                let g = cfg
                    .battery
                    .as_ref()
                    .and_then(|b| b.limit_currents.as_deref())
                    .unwrap_or(&[1500000, 1200000, 900000, 600000, 300000, 0]);
                let c: Vec<i32> = t.iter().map(|v| v - 4000).collect();
                StepPolicy::new("CHG_BAT", t, &c, g)
            },
            chg_skin: StepPolicy::new(
                "CHG_SKIN",
                &[46000, 49000, 52000, 55000],
                &[43000, 46000, 49000, 52000],
                &[1500000, 1200000, 900000, 600000],
            ),
            restrict_level: cfg.battery.as_ref().and_then(|b| b.restrict_level).unwrap_or(3) as usize,
            suspend_level: cfg.battery.as_ref().and_then(|b| b.suspend_level).unwrap_or(4) as usize,
            clamp_temps: cfg
                .battery
                .as_ref()
                .and_then(|b| b.clamp_temps.clone())
                .unwrap_or_else(|| vec![34500, 36000, 37500, 39000]),
            clamp_currents: cfg
                .battery
                .as_ref()
                .and_then(|b| b.clamp_currents.clone())
                .unwrap_or_else(|| vec![1000000, 600000, 350000, 150000]),
            max_fcc: 4250000,
            voice_on: false,
            wifi_on: false,
            brightness_on: false,
            modem_on: false,
            chg_level: 0,
            suspend_on: false,
            last_ccc: -1,
            framework_ccc,
            last_board_written: i32::MIN,
            last_bright_written: 255,
            gpu_freq_table: Self::parse_gpu_freqs(cfg),
            last_batt_temp: i32::MIN,
            chg_rate_ema: 0.0,
            chg_boost_floor: cfg
                .battery
                .as_ref()
                .and_then(|b| b.boost_floor)
                .unwrap_or(0),
        }
    }

    fn parse_gpu_freqs(cfg: &config::ThermalConfig) -> Vec<i32> {
        if let Some(ref gpu) = cfg.gpu {
            if let Some(ref freqs) = gpu.freqs {
                let mut sorted = freqs.clone();
                sorted.sort_unstable_by(|a, b| b.cmp(a));
                return sorted;
            }
        }
        vec![1100000000, 950000000, 835000000, 736000000, 684000000, 633000000, 500000000, 353000000, 255000000]
    }

    fn apply(&mut self, ctl: &mut Controls, base: &Baselines, r: &sensors::Reading, kmsg_events: &mut Vec<String>) {
        let v = r.virtual_temp;
        let gpu_v = r.gpu_therm;

        let cpu0_v = r.cpu0_junction.saturating_sub(JUNCTION_OFFSET).max(r.cpu_therm);
        let cpu3_v = r.cpu3_junction.saturating_sub(JUNCTION_OFFSET).max(r.cpu_therm);
        let cpu7_v = r.cpu7_junction.saturating_sub(JUNCTION_OFFSET).max(r.cpu_therm);
        let cpu_v = cpu0_v.max(cpu3_v).max(cpu7_v);

        if let Some((lvl, freq)) = self.cpu0.update(cpu0_v) {
            let f = if lvl == 0 { base.cpu0_max } else { freq };
            if let Some(n) = ctl.cpu_limits.as_mut() {
                n.write_raw(&format!("cpu0 {}\n", f));
            }
            kmsg_events.push(format!("cpu0 L{} {}Hz", lvl, f));
        }
        if let Some((lvl, freq)) = self.cpu3.update(cpu3_v) {
            let f = if lvl == 0 { base.cpu3_max } else { freq };
            if let Some(n) = ctl.cpu_limits.as_mut() {
                n.write_raw(&format!("cpu3 {}\n", f));
            }
            kmsg_events.push(format!("cpu3 L{} {}Hz", lvl, f));
        }
        if let Some((lvl, freq)) = self.cpu7.update(cpu7_v) {
            let f = if lvl == 0 { base.cpu7_max } else { freq };
            if let Some(n) = ctl.cpu_limits.as_mut() {
                n.write_raw(&format!("cpu7 {}\n", f));
            }
            kmsg_events.push(format!("cpu7 L{} {}Hz", lvl, f));
        }
        if let Some((lvl, freq)) = self.gpu.update(gpu_v) {
            let f = if lvl == 0 { base.gpu_max } else { freq };
            if let Some(n) = ctl.gpu_max_freq.as_mut() {
                n.write_i32(f);
            }
            kmsg_events.push(format!("gpu L{} {}Hz", lvl, f));
        }
        if let Some((lvl, st)) = self.cdsp.update(cpu_v) {
            let s = if lvl == 0 { 0 } else { st };
            if let Some(n) = ctl.cdsp.as_mut() {
                n.write_i32(s);
            }
            kmsg_events.push(format!("cdsp L{} state{}", lvl, s));
        }
        if let Some((lvl, st)) = self.lmh.update(cpu_v) {
            let s = if lvl == 0 { 0 } else { st };
            if let Some(n) = ctl.lmh.as_mut() {
                n.write_i32(s);
            }
            kmsg_events.push(format!("lmh L{} state{}", lvl, s));
        }
        if let Some((lvl, st)) = self.hotplug.update(cpu_v) {
            let s = if lvl == 0 { 0 } else { st };
            for c in [&mut ctl.pause_cpu3, &mut ctl.pause_cpu4, &mut ctl.pause_cpu7] {
                if let Some(n) = c {
                    n.write_i32(s);
                }
            }
            kmsg_events.push(format!("hotplug L{} state{}", lvl, s));
        }
        if let Some((lvl, val)) = self.tstate.update(cpu_v) {
            let x = if lvl == 0 { 0 } else { val };
            if let Some(n) = ctl.temp_state.as_mut() {
                n.write_i32(x);
            }
        }
        if let Some((lvl, val)) = self.torch.update(r.flash_temp) {
            let x = if lvl == 0 { 0 } else { val };
            if let Some(n) = ctl.torch_level.as_mut() {
                n.write_i32(x);
            }
            kmsg_events.push(format!("torch level{}", x));
        }

        let wifi_v = r.wifi_therm.max(r.hotspot_temp);
        if let Some((lvl, val)) = self.wifi.update(wifi_v) {
            let on = lvl > 0;
            if on != self.wifi_on {
                self.wifi_on = on;
                if let Some(n) = ctl.wifi_limit.as_mut() {
                    n.write_i32(if on { val } else { 0 });
                }
                kmsg_events.push(format!("wifi {}", if on { "lim" } else { "free" }));
            }
        }

        if let Some((lvl, val)) = self.brightness.update(v) {
            let x = if lvl == 0 { 255 } else { val };
            if x != self.last_bright_written {
                self.last_bright_written = x;
                if let Some(n) = ctl.max_brightness.as_mut() {
                    n.write_i32(x);
                }
                kmsg_events.push(format!("brightness {}", x));
            }
        }

        if let Some((lvl, val)) = self.modem.update(r.modem_temp) {
            let on = lvl > 0;
            if on != self.modem_on {
                self.modem_on = on;
                if let Some(n) = ctl.modem_limit.as_mut() {
                    n.write_i32(if on { val } else { 0 });
                }
                kmsg_events.push(format!("modem {}", if on { "lim" } else { "free" }));
            }
        }

        let voice_want = v >= 51000;
        if voice_want != self.voice_on {
            self.voice_on = voice_want;
            if let Some(n) = ctl.voice_limit.as_mut() {
                n.write_i32(if voice_want { 1 } else { 0 });
            }
        }

        if (v - self.last_board_written).abs() > 200 || self.last_board_written == i32::MIN {
            if let Some(n) = ctl.board_temp.as_mut() {
                n.write_i32(v);
            }
            if let Some(n) = ctl.board_other_temp.as_mut() {
                n.write_i32(v);
            }
            self.last_board_written = v;
        }

        let tenths = (v / 100).clamp(250, 550);
        if let Some(n) = ctl.thermal_board_temp.as_mut() {
            n.write_i32(tenths);
        }

        self.apply_charging(ctl, r, v, kmsg_events);
    }














    fn thermal_fcc_ramp(&self, temp_mc: i32) -> i32 {
        let cap = self.max_fcc;
        let t = temp_mc as f32 / 1000.0;
        if t < 42.0 {
            return cap;
        }
        if t >= 50.0 {
            return 0;
        }

        let pct = 1.0 - (t - 42.0) / 8.0;
        let mut fcc = (cap as f32 * pct.clamp(0.0, 1.0)) as i32;
        if self.chg_boost_floor > 0 && fcc < self.chg_boost_floor && t < 50.0 {
            fcc = self.chg_boost_floor;
        }
        fcc
    }







    #[allow(dead_code)]
    fn thermal_fcc_rate_cap(&self, cur_fcc: i32) -> i32 {
        cur_fcc
    }

    fn apply_charging(&mut self, ctl: &mut Controls, r: &sensors::Reading, _vtemp: i32, kmsg_events: &mut Vec<String>) {
        let online = ctl.usb_online.as_mut()
            .and_then(|n| n.read_i32())
            .unwrap_or(0) != 0;

        if !online {
            kmsg_events.push("CHG_OFFLINE".into());
            return;
        }

        let bt = r.battery_temp;
        if self.last_batt_temp != i32::MIN {
            let raw = (bt - self.last_batt_temp) as f32 / 1000.0;
            self.chg_rate_ema = 0.3 * raw + 0.7 * self.chg_rate_ema;
        }
        self.last_batt_temp = bt;

        let temp_c = bt as f32 / 1000.0;
        let mut fcc = self.thermal_fcc_ramp(bt);
        fcc = self.thermal_fcc_rate_cap(fcc);

        kmsg_events.push(format!(
            "CHG_BT={:.1} fcc={}/{} rate={:.3}",
            temp_c, fcc, self.max_fcc, self.chg_rate_ema
        ));

        if let Some(n) = ctl.fcc.as_mut() {
            n.write_i32(fcc);
        }
    }
}

pub fn klog(msg: &str) {
    println!("hoshikv-thermal: {}", msg);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .append(true)
        .open("/dev/kmsg")
    {
        let _ = f.write_all(format!("<6>hoshikv-thermal: {}\n", msg).as_bytes());
    }
}

fn write_status(lines: &[String]) {
    let _ = std::fs::write(DUMP_PATH, lines.join("\n"));
}

extern "C" fn on_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn binder_thread(shared: std::sync::Arc<hal::SharedState>) {
    let driver = if std::path::Path::new(rsbinder::DEFAULT_BINDER_PATH).exists() {
        rsbinder::DEFAULT_BINDER_PATH
    } else {
        rsbinder::LEGACY_BINDER_PATH
    };

    if let Err(e) = rsbinder::ProcessState::init(driver, 4) {
        klog(&format!("binder init failed: {:?}", e));
        return;
    }

    let service = hal::aidl_bindings::android::hardware::thermal::IThermal::BnThermal::new_binder(
        hal::ThermalServiceImpl {
            shared: shared.clone(),
        },
    );

    let mut attempt = 0;
    loop {
        match rsbinder::hub::add_service(hal::INSTANCE, service.clone()) {
            Ok(_) => {
                klog("IThermal/default registered");
                break;
            }
            Err(e) => {
                attempt += 1;
                if attempt % 5 == 1 {
                    klog(&format!("waiting for IThermal/default slot: {:?}", e));
                }
                if SHUTDOWN.load(Ordering::SeqCst) || attempt > 150 {
                    return;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }

    rsbinder::ProcessState::join_thread_pool().ok();
}

fn main() {
    unsafe {
        libc::signal(SIGTERM, on_signal as *const () as usize);
        libc::signal(SIGINT, on_signal as *const () as usize);
        libc::prctl(libc::PR_SET_NAME, b"hoshikv-thermal\0".as_ptr(), 0, 0, 0);
    }

    klog("hoshikv-thermal engine v2 :D");

    let cfg = config::ThermalConfig::load();
    klog("config loaded");

    let shared = hal::SharedState::new();
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("binder-pool".into())
            .spawn(move || binder_thread(shared))
            .ok();
    }

    let mut weights = Weights::default();
    cfg.apply_weights(&mut weights);

    let mut sensors_set = SensorSet::probe();
    sensors_set.log_missing();

    let baselines = probe_baselines();
    klog(&format!(
        "baselines: cpu0={} cpu3={} cpu7={} gpu={}",
        baselines.cpu0_max, baselines.cpu3_max, baselines.cpu7_max, baselines.gpu_max
    ));

    let mut ctl = Controls::probe();
    let missing = [
        ("cpu_limits", ctl.cpu_limits.is_none()),
        ("gpu_max_freq", ctl.gpu_max_freq.is_none()),
        ("gpu_cdev", ctl.gpu_cdev.is_none()),
        ("cdsp_cdev", ctl.cdsp.is_none()),
        ("lmh_cdev", ctl.lmh.is_none()),
        ("battery_cdev", ctl.battery_cdev.is_none()),
        ("pause3", ctl.pause_cpu3.is_none()),
    ];
    for (name, miss) in missing.iter() {
        if *miss {
            klog(&format!("warning: control node unavailable: {}", name));
        }
    }

    ctl.release_all(&baselines);
    let _ = std::fs::set_permissions("/sys/class/qcom-battery/input_suspend",
        std::os::unix::fs::PermissionsExt::from_mode(0o666));
    let usb_in = SysfsNode::open("/sys/class/power_supply/usb/input_current_limit", false)
        .and_then(|mut n| n.read_i32())
        .unwrap_or(0);
    let framework_ccc = if usb_in > 50000 { usb_in } else { 2853000 };
    let mut engine = ThermalEngine::new(framework_ccc, &cfg, &baselines);

    let power_cfg = cfg.power.clone().unwrap_or_default();
    let mut tuner = Tuner::new(&power_cfg, baselines.cpu0_max);
    let tuner_desc = tuner.freq_table();
    if !power_cfg.governor.is_none()
        || power_cfg.swappiness.is_some()
        || power_cfg.max_freq_p0.is_some()
        || power_cfg.max_freq_p3.is_some()
        || power_cfg.max_freq_p6.is_some()
        || power_cfg.max_freq_p7.is_some()
    {
        klog(&format!(
            "power tuning enabled: freq[{}] gov={:?} swap={:?}",
            tuner_desc,
            power_cfg.governor,
            power_cfg.swappiness
        ));
    }

    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut tick: u32 = 0;
    let mut fail_ticks: u32 = 0;
    let mut failsafe_active = false;
    let mut history: Vec<String> = Vec::with_capacity(DUMP_MAX_LINES);
    let mut load_tracker = LoadTracker::new();
    let mut thermal_guard = ThermalGuard::new();

    while !SHUTDOWN.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(TICK_MS));
        tick = tick.wrapping_add(1);

        let reading = sensors_set.read_all(&weights);
        let sane = reading.virtual_temp > 0 && reading.virtual_temp < 90000;
        let present = sensors_set.all_present();

        if !present || !sane {
            fail_ticks += 1;
        } else {
            fail_ticks = 0;
            if failsafe_active {
                failsafe_active = false;
                klog("sensors recovered, resuming normal operation");
            }
        }

        if fail_ticks >= 10 && !failsafe_active {
            failsafe_active = true;
            klog("sensor failure persists, entering failsafe (limits released)");
            ctl.release_all(&baselines);
        }

        let mut events: Vec<String> = Vec::new();

        if !failsafe_active {
            let load = load_tracker.sample(reading.screen_on, reading.cpu_therm);
            let risk = thermal_guard.evaluate(reading.cpu_therm, reading.virtual_temp, reading.battery_temp);
            thermal_guard.maybe_rearm();

            if risk >= ThermalRisk::Critical {
                events.push(format!("GUARD {:?} rate={:.1} prot={}", risk, thermal_guard.rate(), thermal_guard.protection_on));
                let cap = thermal_guard.freq_cap_for(risk, load, baselines.cpu0_max);
                if let Some(n) = ctl.cpu_limits.as_mut() {
                    n.write_raw(&format!("cpu0 {}\n", cap));
                    n.write_raw(&format!("cpu3 {}\n", cap));
                    n.write_raw(&format!("cpu7 {}\n", cap));
                }
            } else {
                engine.apply(&mut ctl, &baselines, &reading, &mut events);
            }

            if tick % 8 == 0 {
                events.push(format!("LD:{} R:{:?} r={:.1}", load.label(), risk, thermal_guard.rate()));
            }
        }

        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(start_time)
            .saturating_sub(start_time);
        tuner.apply(elapsed, reading.virtual_temp);

        for e in &events {
            klog(e);
        }

        {
            use std::collections::HashMap;
            let mut zone_paths: HashMap<String, String> = HashMap::new();
            let mut zone_reader = |z: &str| -> Option<f32> {
                let path = match zone_paths.get(z) {
                    Some(p) => p.clone(),
                    None => {
                        let p = sysfs::find_zone_by_type(z)?;
                        zone_paths.insert(z.to_string(), p.clone());
                        p
                    }
                };
                std::fs::read_to_string(&path).ok()?.trim().parse::<f32>().ok()
            };
            let _hal_events =
                hal::update_from_reading(&shared, &reading, &mut zone_reader);

            let mut gpu_c = 0i64;
            if let Some(n) = ctl.gpu_cdev.as_mut() {
                gpu_c = n.read_i32().unwrap_or(0) as i64;
            }
            let mut bat_c = 0i64;
            if let Some(n) = ctl.battery_cdev.as_mut() {
                bat_c = n.read_i32().unwrap_or(0) as i64;
            }
            let mut lmh_c = 0i64;
            if let Some(n) = ctl.lmh.as_mut() {
                lmh_c = n.read_i32().unwrap_or(0) as i64;
            }
            let mut pause3_c = 0i64;
            if let Some(n) = ctl.pause_cpu3.as_mut() {
                pause3_c = n.read_i32().unwrap_or(0) as i64;
            }
            let cpu0_c = 0i64;
            hal::refresh_cooling_devices(
                &shared,
                &[
                    ("cpufreq-cpu0", cpu0_c),
                    ("cpufreq-cpu3", lmh_c),
                    ("cpufreq-cpu7", pause3_c),
                    ("gpu", gpu_c),
                    ("battery", bat_c),
                ],
            );
        }

        if tick % STATUS_INTERVAL_TICKS == 0 {
            let gpu_freq = ctl.gpu_max_freq.as_mut()
                .and_then(|n| n.read_i32())
                .unwrap_or(0);

            let line = format!(
                "{} V:{} B:{} C0:L{} C3:L{} C7:L{} G:L{}Hz NP:L{} LMH:L{} HP:L{} TS:L{} SOC:{}% CHG:L{} {} FS:{} SCR:{} LD:{} {} | cpu:{} jn0:{} jn3:{} jn7:{} chg:{} wifi:{} pa0:{} pa1:{} quiet:{} conn:{} md:{} hs:{} PW:{}",
                chrono_fmt(),
                reading.virtual_temp as f32 / 1000.0,
                reading.battery_temp as f32 / 1000.0,
                engine.cpu0.level(),
                engine.cpu3.level(),
                engine.cpu7.level(),
                gpu_freq / 1000000,
                engine.cdsp.level(),
                engine.lmh.level(),
                engine.hotplug.level(),
                engine.tstate.level(),
                reading.soc,
                engine.chg_level,
                if engine.suspend_on { "SUSPEND" } else { "" },
                if failsafe_active { "Y" } else { "N" },
                if reading.screen_on { "on" } else { "off" },
                load_tracker.get().label(),
                thermal_guard.snapshot(),
                reading.cpu_therm / 1000,
                reading.cpu0_junction / 1000,
                reading.cpu3_junction / 1000,
                reading.cpu7_junction / 1000,
                reading.charger_therm / 1000,
                reading.wifi_therm / 1000,
                reading.pa0_temp / 1000,
                reading.pa1_temp / 1000,
                reading.quiet_temp / 1000,
                reading.conn_temp / 1000,
                reading.modem_temp / 1000,
                reading.hotspot_temp / 1000,
                tuner.freq_table(),
            );
            println!("{}", line);
            history.push(line);
            if history.len() > DUMP_MAX_LINES {
                history.remove(0);
            }
            write_status(&history);
        }
    }

    klog("shutting down, releasing controls");
    ctl.release_all(&baselines);
}

fn chrono_fmt() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs_of_day = now % 86400;
    format!(
        "{:02}:{:02}:{:02}",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

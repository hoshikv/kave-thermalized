use crate::sensors::Weights;
use serde::Deserialize;
use std::path::Path;

const DEFAULT_PATH: &str = "/vendor/etc/thermal_config.json";
const FALLBACK_PATH: &str = "/data/vendor/thermal/thermal_config.json";

#[allow(dead_code)]
#[derive(Deserialize, Default)]
pub struct ThermalConfig {
    pub weights: Option<WeightCfg>,
    pub cpu0: Option<PolicyCfg>,
    pub cpu3: Option<PolicyCfg>,
    pub cpu7: Option<PolicyCfg>,
    pub gpu: Option<GpuCfg>,
    pub battery: Option<BatteryCfg>,
    pub hal: Option<HalCfg>,
    pub power: Option<PowerCfg>,
}

#[allow(dead_code)]
#[derive(Deserialize, Default, Clone)]
pub struct PowerCfg {
    pub max_freq_p0: Option<i32>,
    pub max_freq_p3: Option<i32>,
    pub max_freq_p6: Option<i32>,
    pub max_freq_p7: Option<i32>,




    pub boot_guard_s: Option<u64>,
    pub boot_max_p0: Option<i32>,
    pub boot_max_p3: Option<i32>,
    pub boot_max_p6: Option<i32>,
    pub boot_max_p7: Option<i32>,
    pub governor: Option<String>,
    pub swappiness: Option<i32>,
    pub gate_temp: Option<i32>,
    pub props: Option<Vec<PropCfg>>,
    pub boost: Option<i32>,
    pub dirty_ratio: Option<i32>,
    pub dirty_bg: Option<i32>,
}

#[derive(Deserialize, Default, Clone)]
pub struct PropCfg {
    pub name: String,
    pub value: String,
}

#[derive(Deserialize, Default)]
pub struct WeightCfg {
    pub cpu: Option<i32>,
    pub batt: Option<i32>,
    pub chg: Option<i32>,
    pub wifi: Option<i32>,
    pub pa0: Option<i32>,
    pub pa1: Option<i32>,
    pub quiet: Option<i32>,
    pub conn: Option<i32>,
    pub hotspot: Option<i32>,
}

#[derive(Deserialize, Default)]
pub struct PolicyCfg {
    pub trig: Option<Vec<i32>>,
    pub clr: Option<Vec<i32>>,
    pub targets: Option<Vec<i32>>,
}

#[derive(Deserialize, Default)]
pub struct GpuCfg {
    pub trig: Option<Vec<i32>>,
    pub clr: Option<Vec<i32>>,
    pub freqs: Option<Vec<i32>>,
}

#[allow(dead_code)]
#[derive(Deserialize, Default)]
pub struct BatteryCfg {
    pub limit_temps: Option<Vec<i32>>,
    pub limit_currents: Option<Vec<i32>>,
    pub restrict_level: Option<i32>,
    pub suspend_level: Option<i32>,
    pub clamp_temps: Option<Vec<i32>>,
    pub clamp_currents: Option<Vec<i32>>,



    pub boost_floor: Option<i32>,
}

#[allow(dead_code)]
#[derive(Deserialize, Default)]
pub struct HalCfg {
    pub skin_hot: Option<Vec<i32>>,
}

impl ThermalConfig {
    pub fn load() -> Self {
        let try_load = |path: &str| -> Option<Self> {
            let p = Path::new(path);
            if !p.exists() {
                return None;
            }
            let data = std::fs::read_to_string(p).ok()?;
            serde_json::from_str(&data).ok()
        };

        let mut cfg = try_load(FALLBACK_PATH)
            .or_else(|| try_load(DEFAULT_PATH))
            .unwrap_or_default();

        let stale = cfg.power.is_none() && cfg.cpu0.is_none() && cfg.battery.is_none();
        if stale {
            let seed = f5mimic_config_json();
            let _ = std::fs::create_dir_all("/data/vendor/thermal");
            if std::fs::write(FALLBACK_PATH, seed.as_bytes()).is_ok() {
                let _ = std::fs::set_permissions(
                    FALLBACK_PATH,
                    std::os::unix::fs::PermissionsExt::from_mode(0o644),
                );
                cfg = serde_json::from_str(&seed).unwrap_or_default();
            }
        }
        cfg
    }

    pub fn apply_weights(&self, w: &mut Weights) {
        if let Some(ref wc) = self.weights {
            if let Some(v) = wc.cpu { w.cpu = v; }
            if let Some(v) = wc.batt { w.batt = v; }
            if let Some(v) = wc.chg { w.chg = v; }
            if let Some(v) = wc.wifi { w.wifi = v; }
            if let Some(v) = wc.pa0 { w.pa0 = v; }
            if let Some(v) = wc.pa1 { w.pa1 = v; }
            if let Some(v) = wc.quiet { w.quiet = v; }
            if let Some(v) = wc.conn { w.conn = v; }
            if let Some(v) = wc.hotspot { w.hotspot = v; }
        }
    }
}

pub fn f5mimic_config_json() -> String {
    r#"{
    "weights": {"cpu":200,"batt":200,"chg":100,"wifi":60,"pa0":40,"pa1":80,"quiet":150,"conn":100,"hotspot":60},
    "cpu0":  {"trig":[66000,72000,78000,84000,90000],"clr":[60000,66000,72000,78000,84000],"targets":[1651200,1401600,1209600,1017600,902400]},
    "cpu3":  {"trig":[66000,72000,78000,84000,90000,95000,99000],"clr":[60000,66000,72000,78000,84000,89000,93000],"targets":[1900800,1651200,1459200,1248000,1056000,902400,787200]},
    "cpu7":  {"trig":[66000,72000,78000,84000,90000,95000,99000],"clr":[60000,66000,72000,78000,84000,89000,93000],"targets":[1814400,1564800,1363200,1152000,960000,844800,691200]},
    "gpu":   {"trig":[68000,75000,82000,89000,95000,100000],"clr":[62000,69000,76000,83000,89000,94000],"freqs":[633000000,500000000,400000000,300000000,220000000,150000000]},
    "battery":{"limit_temps":[46000,48000,50000,52000,54000,56000],"limit_currents":[1500000,1200000,900000,600000,300000,0],"restrict_level":3,"suspend_level":4,"clamp_temps":[34500,36000,37500,39000],"clamp_currents":[1000000,600000,350000,150000]},
    "power": {
        "max_freq_p0":1843200,"max_freq_p3":2572800,"max_freq_p6":0,"max_freq_p7":2918400,
        "boot_guard_s":90,"boot_max_p0":1478400,"boot_max_p3":2000000,"boot_max_p6":0,"boot_max_p7":2265600,
        "governor":"walt","swappiness":100,"gate_temp":70000,
        "boost":0,"dirty_ratio":20,"dirty_bg":5,
        "props":[
            {"name":"persist.vendor.disable_idle_fps","value":"false"},
            {"name":"vendor.dfps.enable","value":"true"},
            {"name":"debug.vendor.perf.smart_touch","value":"0"},
            {"name":"vendor.perf.framepacing.enable","value":"0"}
        ]
    }
}
"#
    .to_string()
}

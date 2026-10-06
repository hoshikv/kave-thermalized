use crate::sensors::Reading;
use std::sync::{Arc, Mutex, RwLock};

pub mod aidl_bindings {
    include!(concat!(env!("OUT_DIR"), "/thermal_aidl.rs"));
}

use aidl_bindings::android::hardware::thermal::CoolingDevice::CoolingDevice;
use aidl_bindings::android::hardware::thermal::CoolingType::CoolingType;
use aidl_bindings::android::hardware::thermal::IThermal::IThermal;
use aidl_bindings::android::hardware::thermal::IThermalChangedCallback::IThermalChangedCallback;
use aidl_bindings::android::hardware::thermal::Temperature::Temperature;
use aidl_bindings::android::hardware::thermal::TemperatureThreshold::TemperatureThreshold;
use aidl_bindings::android::hardware::thermal::TemperatureType::TemperatureType;
use aidl_bindings::android::hardware::thermal::ThrottlingSeverity::ThrottlingSeverity;

pub const INSTANCE: &str = "android.hardware.thermal.IThermal/default";

struct CallbackEntry {
    cb: Strong<dyn IThermalChangedCallback>,
    filter: Option<i32>,
}

type Strong<T> = rsbinder::Strong<T>;

#[derive(Default)]
struct CallbackRegistry {
    entries: Mutex<Vec<CallbackEntry>>,
}

pub struct SharedState {
    pub temperatures: RwLock<Vec<Temperature>>,
    pub thresholds: Vec<TemperatureThreshold>,
    pub cooling_devices: RwLock<Vec<CoolingDevice>>,
    pub callbacks: CallbackRegistry,
}

impl SharedState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            temperatures: RwLock::new(Vec::new()),
            thresholds: build_thresholds(),
            cooling_devices: RwLock::new(Vec::new()),
            callbacks: CallbackRegistry {
                entries: Mutex::new(Vec::new()),
            },
        })
    }
}

fn sev_to_int(s: &ThrottlingSeverity) -> i32 {
    match *s {
        ThrottlingSeverity::LIGHT => 1,
        ThrottlingSeverity::MODERATE => 2,
        ThrottlingSeverity::SEVERE => 3,
        ThrottlingSeverity::CRITICAL => 4,
        ThrottlingSeverity::EMERGENCY => 5,
        ThrottlingSeverity::SHUTDOWN => 6,
        _ => 0,
    }
}

fn ttype_to_int(t: &TemperatureType) -> i32 {
    match *t {
        TemperatureType::CPU => 0,
        TemperatureType::GPU => 1,
        TemperatureType::BATTERY => 2,
        TemperatureType::SKIN => 3,
        TemperatureType::USB_PORT => 4,
        TemperatureType::POWER_AMPLIFIER => 5,
        TemperatureType::BCL_VOLTAGE => 6,
        TemperatureType::BCL_CURRENT => 7,
        TemperatureType::BCL_PERCENTAGE => 8,
        TemperatureType::NPU => 9,
        _ => -1,
    }
}

fn sev_enum(v: i32) -> ThrottlingSeverity {
    match v {
        1 => ThrottlingSeverity::LIGHT,
        2 => ThrottlingSeverity::MODERATE,
        3 => ThrottlingSeverity::SEVERE,
        4 => ThrottlingSeverity::CRITICAL,
        5 => ThrottlingSeverity::EMERGENCY,
        6 => ThrottlingSeverity::SHUTDOWN,
        _ => ThrottlingSeverity::NONE,
    }
}

struct SensorSpec {
    name: &'static str,
    ty: TemperatureType,
    kind: SensorKind,
    hot: [i32; 6],
}

#[derive(Clone, Copy)]
enum SensorKind {
    Zone(&'static str),
    VirtualSkin,
    BatteryPs,
    Socd,
    Vbat,
}

static SPECS: &[SensorSpec] = &[
    SensorSpec { name: "CPU0", ty: TemperatureType::CPU, kind: SensorKind::Zone("cpu-0-0-0"), hot: [70000, 80000, 90000, 98000, 105000, 115000] },
    SensorSpec { name: "CPU1", ty: TemperatureType::CPU, kind: SensorKind::Zone("cpu-0-1-0"), hot: [70000, 80000, 90000, 98000, 105000, 115000] },
    SensorSpec { name: "CPU2", ty: TemperatureType::CPU, kind: SensorKind::Zone("cpu-0-2-0"), hot: [70000, 80000, 90000, 98000, 105000, 115000] },
    SensorSpec { name: "CPU3", ty: TemperatureType::CPU, kind: SensorKind::Zone("cpu-1-0-0"), hot: [70000, 80000, 90000, 98000, 105000, 115000] },
    SensorSpec { name: "CPU4", ty: TemperatureType::CPU, kind: SensorKind::Zone("cpu-1-1-0"), hot: [70000, 80000, 90000, 98000, 105000, 115000] },
    SensorSpec { name: "CPU5", ty: TemperatureType::CPU, kind: SensorKind::Zone("cpu-1-2-0"), hot: [70000, 80000, 90000, 98000, 105000, 115000] },
    SensorSpec { name: "CPU6", ty: TemperatureType::CPU, kind: SensorKind::Zone("cpu-1-3-0"), hot: [70000, 80000, 90000, 98000, 105000, 115000] },
    SensorSpec { name: "CPU7", ty: TemperatureType::CPU, kind: SensorKind::Zone("cpu-2-0-0"), hot: [70000, 80000, 90000, 98000, 105000, 115000] },
    SensorSpec { name: "GPU0", ty: TemperatureType::GPU, kind: SensorKind::Zone("gpuss-0"), hot: [60000, 70000, 80000, 90000, 100000, 110000] },
    SensorSpec { name: "GPU1", ty: TemperatureType::GPU, kind: SensorKind::Zone("gpuss-1"), hot: [60000, 70000, 80000, 90000, 100000, 110000] },
    SensorSpec { name: "GPU2", ty: TemperatureType::GPU, kind: SensorKind::Zone("gpuss-2"), hot: [60000, 70000, 80000, 90000, 100000, 110000] },
    SensorSpec { name: "GPU3", ty: TemperatureType::GPU, kind: SensorKind::Zone("gpuss-3"), hot: [60000, 70000, 80000, 90000, 100000, 110000] },
    SensorSpec { name: "nsp0", ty: TemperatureType::NPU, kind: SensorKind::Zone("nsphvx-0"), hot: [65000, 75000, 85000, 95000, 105000, 115000] },
    SensorSpec { name: "nsp1", ty: TemperatureType::NPU, kind: SensorKind::Zone("nsphvx-1"), hot: [65000, 75000, 85000, 95000, 105000, 115000] },
    SensorSpec { name: "nsp2", ty: TemperatureType::NPU, kind: SensorKind::Zone("nsphmx-0"), hot: [65000, 75000, 85000, 95000, 105000, 115000] },
    SensorSpec { name: "nsp3", ty: TemperatureType::NPU, kind: SensorKind::Zone("nsphmx-1"), hot: [65000, 75000, 85000, 95000, 105000, 115000] },
    SensorSpec { name: "battery", ty: TemperatureType::BATTERY, kind: SensorKind::BatteryPs, hot: [43000, 45000, 48000, 50000, 52000, 55000] },
    SensorSpec { name: "skin", ty: TemperatureType::SKIN, kind: SensorKind::VirtualSkin, hot: [46000, 50000, 54000, 58000, 62000, 66000] },
    SensorSpec { name: "shell_skin", ty: TemperatureType::SKIN, kind: SensorKind::VirtualSkin, hot: [46000, 50000, 54000, 58000, 62000, 66000] },
    SensorSpec { name: "socd", ty: TemperatureType::BCL_PERCENTAGE, kind: SensorKind::Socd, hot: [65000, 75000, 85000, 95000, 105000, 115000] },
    SensorSpec { name: "vbat", ty: TemperatureType::BCL_VOLTAGE, kind: SensorKind::Vbat, hot: [4200, 4300, 4400, 4500, 4600, 4700] },
];

fn compute_severity(value_mc: i32, hot: &[i32; 6]) -> i32 {
    let mut s = 0;
    for (i, th) in hot.iter().enumerate() {
        if value_mc >= *th {
            s = i as i32 + 1;
        }
    }
    s
}

fn make_temperature(spec: &SensorSpec, raw_mc: i32, value_f32: f32) -> Temperature {
    Temperature {
        r#type: spec.ty,
        name: spec.name.to_string(),
        value: value_f32,
        throttlingStatus: sev_enum(compute_severity(raw_mc, &spec.hot)),
    }
}

pub fn update_from_reading(
    shared: &Arc<SharedState>,
    r: &Reading,
    zone_reader: &mut dyn FnMut(&str) -> Option<f32>,
) -> Vec<String> {
    let events = Vec::new();
    let mut temps = shared.temperatures.write().unwrap();

    let mut new_list: Vec<Temperature> = Vec::with_capacity(SPECS.len());
    let mut severities: Vec<(&'static str, i32)> = Vec::new();

    for spec in SPECS.iter() {
        let (raw_mc, value_f32): (i32, f32) = match spec.kind {
            SensorKind::Zone(z) => {
                let v = zone_reader(z).unwrap_or(25000.0);
                (v as i32, v / 1000.0)
            }
            SensorKind::VirtualSkin => (r.virtual_temp, r.virtual_temp as f32 / 1000.0),
            SensorKind::BatteryPs => (r.battery_temp, r.battery_temp as f32 / 1000.0),
            SensorKind::Socd => {
                let v = zone_reader("socd").unwrap_or(25000.0) as i32;
                let mc = if v > 1000 { v } else { v * 1000 };
                (mc, mc as f32 / 1000.0)
            }
            SensorKind::Vbat => {
                let v = zone_reader("vbat").unwrap_or(3800.0) as i32;
                let mv = if v > 10000 { v / 1000 } else { v };
                (mv, mv as f32 / 1000.0)
            }
        };
        let t = make_temperature(spec, raw_mc, value_f32);
        severities.push((spec.name, sev_to_int(&t.throttlingStatus)));
        new_list.push(t);
    }

    let changed = temps.is_empty()
        || temps
            .iter()
            .zip(new_list.iter())
            .any(|(a, b)| sev_to_int(&a.throttlingStatus) != sev_to_int(&b.throttlingStatus));
    *temps = new_list;
    drop(temps);

    if !changed {
        return events;
    }

    let snapshot: Vec<Temperature> = shared
        .temperatures
        .read()
        .unwrap()
        .iter()
        .map(|t| Temperature {
            r#type: t.r#type,
            name: t.name.clone(),
            value: t.value,
            throttlingStatus: t.throttlingStatus,
        })
        .collect();
    let mut entries = shared.callbacks.entries.lock().unwrap();
    entries.retain_mut(|entry| {
        for (name, _) in &severities {
            if let Some(f) = entry.filter {
                let want_ty = SPECS
                    .iter()
                    .find(|s| s.name == *name)
                    .map(|s| s.ty)
                    .unwrap_or(TemperatureType::UNKNOWN);
                if ttype_to_int(&want_ty) != f {
                    continue;
                }
            }
            if let Some(t) = snapshot.iter().find(|t| t.name == *name) {
                if entry.cb.notifyThrottling(t).is_err() {
                    return false;
                }
            }
        }
        true
    });

    events
}

fn build_thresholds() -> Vec<TemperatureThreshold> {
    SPECS
        .iter()
        .map(|s| TemperatureThreshold {
            coldThrottlingThresholds: vec![],
            hotThrottlingThresholds: s.hot.iter().map(|v| *v as f32 / 1000.0).collect(),
            name: s.name.to_string(),
            r#type: s.ty,
        })
        .collect()
}

pub fn refresh_cooling_devices(shared: &Arc<SharedState>, states: &[(&str, i64)]) {
    let mut list = vec![
        CoolingDevice {
            name: "cpufreq-cpu0".to_string(),
            r#type: CoolingType::CPU,
            value: 0,
        },
        CoolingDevice {
            name: "cpufreq-cpu3".to_string(),
            r#type: CoolingType::CPU,
            value: 0,
        },
        CoolingDevice {
            name: "cpufreq-cpu7".to_string(),
            r#type: CoolingType::CPU,
            value: 0,
        },
        CoolingDevice {
            name: "gpu".to_string(),
            r#type: CoolingType::GPU,
            value: 0,
        },
        CoolingDevice {
            name: "battery".to_string(),
            r#type: CoolingType::BATTERY,
            value: 0,
        },
    ];
    for (name, val) in states {
        if let Some(c) = list.iter_mut().find(|c| c.name == *name) {
            c.value = *val;
        }
    }
    *shared.cooling_devices.write().unwrap() = list;
}

pub struct ThermalServiceImpl {
    pub shared: Arc<SharedState>,
}

impl rsbinder::Interface for ThermalServiceImpl {}

impl IThermal for ThermalServiceImpl {
    fn getCoolingDevices(&self) -> rsbinder::BinderResult<Vec<CoolingDevice>> {
        Ok(self
            .shared
            .cooling_devices
            .read()
            .unwrap()
            .iter()
            .map(|c| CoolingDevice {
                r#type: c.r#type,
                name: c.name.clone(),
                value: c.value,
            })
            .collect())
    }

    fn getCoolingDevicesWithType(
        &self,
        arg_type: CoolingType,
    ) -> rsbinder::BinderResult<Vec<CoolingDevice>> {
        Ok(self
            .shared
            .cooling_devices
            .read()
            .unwrap()
            .iter()
            .filter(|c| c.r#type == arg_type)
            .map(|c| CoolingDevice {
                r#type: c.r#type,
                name: c.name.clone(),
                value: c.value,
            })
            .collect())
    }

    fn getTemperatures(&self) -> rsbinder::BinderResult<Vec<Temperature>> {
        Ok(self
            .shared
            .temperatures
            .read()
            .unwrap()
            .iter()
            .map(|t| Temperature {
                r#type: t.r#type,
                name: t.name.clone(),
                value: t.value,
                throttlingStatus: t.throttlingStatus,
            })
            .collect())
    }

    fn getTemperaturesWithType(
        &self,
        arg_type: TemperatureType,
    ) -> rsbinder::BinderResult<Vec<Temperature>> {
        Ok(self
            .shared
            .temperatures
            .read()
            .unwrap()
            .iter()
            .filter(|t| t.r#type == arg_type)
            .map(|t| Temperature {
                r#type: t.r#type,
                name: t.name.clone(),
                value: t.value,
                throttlingStatus: t.throttlingStatus,
            })
            .collect())
    }

    fn getTemperatureThresholds(&self) -> rsbinder::BinderResult<Vec<TemperatureThreshold>> {
        Ok(self
            .shared
            .thresholds
            .iter()
            .map(|t| TemperatureThreshold {
                r#type: t.r#type,
                name: t.name.clone(),
                hotThrottlingThresholds: t.hotThrottlingThresholds.clone(),
                coldThrottlingThresholds: t.coldThrottlingThresholds.clone(),
            })
            .collect())
    }

    fn getTemperatureThresholdsWithType(
        &self,
        arg_type: TemperatureType,
    ) -> rsbinder::BinderResult<Vec<TemperatureThreshold>> {
        Ok(self
            .shared
            .thresholds
            .iter()
            .filter(|t| t.r#type == arg_type)
            .map(|t| TemperatureThreshold {
                r#type: t.r#type,
                name: t.name.clone(),
                hotThrottlingThresholds: t.hotThrottlingThresholds.clone(),
                coldThrottlingThresholds: t.coldThrottlingThresholds.clone(),
            })
            .collect())
    }

    fn registerThermalChangedCallback(
        &self,
        arg_callback: &Strong<dyn IThermalChangedCallback>,
    ) -> rsbinder::BinderResult<()> {
        self.registerThermalChangedCallbackWithType(arg_callback, TemperatureType::UNKNOWN)
    }

    fn registerThermalChangedCallbackWithType(
        &self,
        arg_callback: &Strong<dyn IThermalChangedCallback>,
        arg_type: TemperatureType,
    ) -> rsbinder::BinderResult<()> {
        let mut entries = self.shared.callbacks.entries.lock().unwrap();
        if entries
            .iter()
            .any(|e| e.cb.as_binder() == arg_callback.as_binder())
        {
            return Err(rsbinder::StatusCode::BadValue.into());
        }
        entries.push(CallbackEntry {
            cb: arg_callback.clone(),
            filter: Some(ttype_to_int(&arg_type)),
        });
        Ok(())
    }

    fn unregisterThermalChangedCallback(
        &self,
        arg_callback: &Strong<dyn IThermalChangedCallback>,
    ) -> rsbinder::BinderResult<()> {
        let mut entries = self.shared.callbacks.entries.lock().unwrap();
        entries.retain(|e| e.cb.as_binder() != arg_callback.as_binder());
        Ok(())
    }
}

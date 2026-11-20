mod algos;
mod config;
mod crypto;
mod devices;
mod hal;
mod mi_thermald;
mod sensors;
mod sysfs;
mod thermal_sensors;

use libc::{SIGINT, SIGTERM};
use sensors::SensorSet;
use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use sysfs::{find_cdev_by_type, find_zone_by_type, RawCache, SysfsNode};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

const TICK_MS: u64 = 1000;

#[cfg(feature = "debug-log")]
const PROCESS_NAME: &str = "hoshikv-thermal-debug";
#[cfg(not(feature = "debug-log"))]
const PROCESS_NAME: &str = "hoshikv-thermal";

pub fn klog(msg: &str) {
    println!("{PROCESS_NAME}: {msg}");
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open("/dev/kmsg") {
        let _ = f.write_all(format!("<6>{PROCESS_NAME}: {msg}\n").as_bytes());
    }
}

extern "C" fn on_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn binder_thread(shared: Arc<hal::SharedState>) {
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

struct PublishCache {
    raw_cache: Arc<RawCache>,
    zones: HashMap<String, Option<SysfsNode>>,
    cooling: [(&'static str, Option<SysfsNode>); 5],
}

impl PublishCache {
    fn new(raw_cache: Arc<RawCache>) -> Self {
        Self {
            raw_cache,
            zones: HashMap::new(),
            cooling: [
                ("cpufreq-cpu0", "cpu-cluster0"),
                ("cpufreq-cpu3", "cpu-cluster1"),
                ("cpufreq-cpu7", "cpu-cluster2"),
                ("gpu", "gpu"),
                ("battery", "battery"),
            ]
            .map(|(name, kind)| {
                (
                    name,
                    find_cdev_by_type(kind).and_then(|path| SysfsNode::open(&path)),
                )
            }),
        }
    }

    fn read_zone(&mut self, zone: &str) -> Option<f32> {
        if let Some(node) = self.zones.get_mut(zone) {
            return node
                .as_mut()?
                .read_i32_cached(Some(&self.raw_cache))
                .map(|value| value as f32);
        }
        let mut node = find_zone_by_type(zone).and_then(|path| SysfsNode::open(&path));
        let value = node
            .as_mut()
            .and_then(|node| node.read_i32_cached(Some(&self.raw_cache)))
            .map(|value| value as f32);
        self.zones.insert(zone.to_owned(), node);
        value
    }

    fn cooling_states(&mut self) -> [(&'static str, i64); 5] {
        let mut states = [("", 0); 5];
        for (state, (name, node)) in states.iter_mut().zip(self.cooling.iter_mut()) {
            let value = node
                .as_mut()
                .and_then(|node| node.read_i32())
                .map(i64::from)
                .unwrap_or(0);
            *state = (*name, value);
        }
        states
    }
}

fn publish(shared: &Arc<hal::SharedState>, sensors_set: &mut SensorSet, cache: &mut PublishCache) {
    let reading = sensors_set.read_all(&sensors::Weights::default());
    hal::update_from_reading(shared, &reading, &mut |zone| cache.read_zone(zone));
    let states = cache.cooling_states();
    hal::refresh_cooling_devices(shared, &states);
}

fn main() {
    unsafe {
        libc::signal(SIGTERM, on_signal as *const () as usize);
        libc::signal(SIGINT, on_signal as *const () as usize);
        let process_name = if cfg!(feature = "debug-log") {
            c"hoshikv-thermal-debug"
        } else {
            c"hoshikv-thermal"
        };
        libc::prctl(libc::PR_SET_NAME, process_name.as_ptr(), 0, 0, 0);
    }

    klog("hoshikv-thermal mi_thermald bridge starting");

    let shared = hal::SharedState::new();
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("binder-pool".into())
            .spawn(move || binder_thread(shared))
            .ok();
    }

    let raw_cache = Arc::new(RawCache::default());
    let mut sensors_set = SensorSet::probe(Arc::clone(&raw_cache));
    sensors_set.log_missing();
    let mut publish_cache = PublishCache::new(Arc::clone(&raw_cache));

    let options = mi_thermald::Options {
        raw_cache: Some(raw_cache),
        ..mi_thermald::Options::default()
    };
    let mut mi_engine = match mi_thermald::Engine::start(options) {
        Ok(engine) => {
            klog("mi_thermald engine started");
            Some(engine)
        }
        Err(error) => {
            klog(&format!("mi_thermald engine failed: {error}"));
            None
        }
    };

    while !SHUTDOWN.load(Ordering::SeqCst) {
        publish(&shared, &mut sensors_set, &mut publish_cache);
        std::thread::sleep(Duration::from_millis(TICK_MS));
    }

    if let Some(engine) = mi_engine.as_mut() {
        engine.stop();
    }
    klog("shutting down");
}

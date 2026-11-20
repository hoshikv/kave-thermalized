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
use sysfs::{find_cdev_by_type, find_zone_by_type, SysfsNode};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

const TICK_MS: u64 = 1000;

pub fn klog(msg: &str) {
    println!("hoshikv-thermal: {}", msg);
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open("/dev/kmsg") {
        let _ = f.write_all(format!("<6>hoshikv-thermal: {}\n", msg).as_bytes());
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

fn read_cdev(name: &str) -> i64 {
    find_cdev_by_type(name)
        .and_then(|path| SysfsNode::open(&path))
        .and_then(|mut node| node.read_i32())
        .map(i64::from)
        .unwrap_or(0)
}

fn publish(shared: &Arc<hal::SharedState>, sensors_set: &mut SensorSet) {
    let reading = sensors_set.read_all(&sensors::Weights::default());
    let mut zone_paths: HashMap<String, String> = HashMap::new();
    let mut zone_reader = |zone: &str| -> Option<f32> {
        let path = match zone_paths.get(zone) {
            Some(path) => path.clone(),
            None => {
                let path = find_zone_by_type(zone)?;
                zone_paths.insert(zone.to_string(), path.clone());
                path
            }
        };
        std::fs::read_to_string(path)
            .ok()?
            .trim()
            .parse::<f32>()
            .ok()
    };

    hal::update_from_reading(shared, &reading, &mut zone_reader);
    hal::refresh_cooling_devices(
        shared,
        &[
            ("cpufreq-cpu0", read_cdev("cpu-cluster0")),
            ("cpufreq-cpu3", read_cdev("cpu-cluster1")),
            ("cpufreq-cpu7", read_cdev("cpu-cluster2")),
            ("gpu", read_cdev("gpu")),
            ("battery", read_cdev("battery")),
        ],
    );
}

fn main() {
    unsafe {
        libc::signal(SIGTERM, on_signal as *const () as usize);
        libc::signal(SIGINT, on_signal as *const () as usize);
        libc::prctl(libc::PR_SET_NAME, c"hoshikv-thermal".as_ptr(), 0, 0, 0);
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

    let mut sensors_set = SensorSet::probe();
    sensors_set.log_missing();

    let mut mi_engine = match mi_thermald::Engine::start(mi_thermald::Options::default()) {
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
        publish(&shared, &mut sensors_set);
        std::thread::sleep(Duration::from_millis(TICK_MS));
    }

    if let Some(engine) = mi_engine.as_mut() {
        engine.stop();
    }
    klog("shutting down");
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use crate::config::{Section, ThermalConfig};
use crate::devices::DeviceStore;
use crate::thermal_sensors::SensorStore;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlgoKind {
    Monitor,
    Ss,
    Sic,
    Chrono,
}

#[derive(Clone, Debug)]
pub struct Algo {
    pub name: String,
    pub kind: AlgoKind,
    pub sensor: String,
    pub devices: Vec<String>,
    pub polling_ms: u64,
    pub trig: Vec<i64>,
    pub clr: Vec<i64>,
    pub targets: Vec<Vec<i64>>,
    pub ks: Vec<i64>,
    pub ki: Vec<i64>,
    pub kc: Vec<i64>,
    pub max: Vec<i64>,
    pub min: Vec<i64>,
    pub proportion: i64,
    pub reverse: bool,
    pub disable: bool,
}

impl Algo {
    pub fn from_config(config: &ThermalConfig) -> Vec<Self> {
        config
            .sections
            .iter()
            .filter_map(Self::from_section)
            .collect()
    }

    fn from_section(section: &Section) -> Option<Self> {
        let kind = match section.get("algo_type")? {
            "monitor" => AlgoKind::Monitor,
            "ss" => AlgoKind::Ss,
            "sic" => AlgoKind::Sic,
            "chrono" => AlgoKind::Chrono,
            _ => return None,
        };
        let sensor = section.get("sensor")?.to_owned();
        let devices = section.plus_words("device");
        if devices.is_empty() {
            return None;
        }
        Some(Self {
            name: section.name.clone(),
            kind,
            sensor,
            devices,
            polling_ms: section.int("polling", 1000).max(100) as u64,
            trig: section.ints("trig"),
            clr: section.ints("clr"),
            targets: parse_targets(section.get("target")),
            ks: section.ints("ks"),
            ki: section.ints("ki"),
            kc: section.ints("kc"),
            max: section.ints("max"),
            min: section.ints("min"),
            proportion: section.int("proportion", 37),
            reverse: section.int("reverse", 0) != 0,
            disable: section.int("disable", 0) != 0,
        })
    }

    fn count(&self) -> usize {
        match self.kind {
            AlgoKind::Monitor => self.trig.len().min(self.clr.len()),
            AlgoKind::Sic => self.trig.len().min(self.clr.len()).min(self.targets.len()),
            _ => self.trig.len(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EvalResult {
    pub temp: i64,
    pub level: i64,
    pub writes: Vec<(String, i64)>,
}

#[derive(Clone, Debug)]
pub struct AlgoState {
    levels: Vec<bool>,
    last_level: i64,
}

impl AlgoState {
    fn new(count: usize) -> Self {
        Self {
            levels: vec![false; count],
            last_level: 0,
        }
    }
}

pub struct WorkerGroup {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    threads: Vec<Thread>,
}

impl WorkerGroup {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in &self.threads {
            thread.unpark();
        }
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

pub fn run_once(algos: Vec<Algo>, sensors: &SensorStore, devices: &DeviceStore, dry_run: bool) {
    for (algo_id, algo) in algos.into_iter().enumerate() {
        let mut state = AlgoState::new(algo.count());
        match evaluate(
            &algo,
            &mut state,
            sensors,
            devices,
            dry_run,
            algo_id as u32,
            true,
        ) {
            Ok(result) => {
                println!(
                    "{} sensor={} temp={} level={} writes={:?}",
                    algo.name, algo.sensor, result.temp, result.level, result.writes
                );
            }
            Err(error) => println!("{} error={error}", algo.name),
        }
    }
}

pub fn start(
    algos: Vec<Algo>,
    sensors: Arc<SensorStore>,
    devices: Arc<DeviceStore>,
    dry_run: bool,
    verbose: bool,
) -> WorkerGroup {
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    let mut threads = Vec::new();
    for (algo_id, algo) in algos.into_iter().enumerate() {
        if algo.disable {
            continue;
        }
        if !matches!(algo.kind, AlgoKind::Monitor | AlgoKind::Ss | AlgoKind::Sic) {
            eprintln!(
                "{}: {:?} is not implemented yet, skipped",
                algo.name, algo.kind
            );
            continue;
        }
        let sensors = Arc::clone(&sensors);
        let devices = Arc::clone(&devices);
        let stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let algo_id = algo_id as u32;
            let polling = algo.polling_ms.max(100);
            let phase = (algo_id as u64)
                .saturating_mul(97)
                .checked_rem(polling)
                .unwrap_or(0);
            let mut state = AlgoState::new(algo.count());
            let mut first = true;
            while !stop.load(Ordering::Relaxed) {
                match evaluate(
                    &algo, &mut state, &sensors, &devices, dry_run, algo_id, verbose,
                ) {
                    Ok(result) => {
                        if verbose {
                            println!(
                                "{} sensor={} temp={} level={} writes={:?}",
                                algo.name, algo.sensor, result.temp, result.level, result.writes
                            );
                        }
                    }
                    Err(error) => eprintln!("{}: {error}", algo.name),
                }
                let delay = if first {
                    first = false;
                    polling.saturating_add(phase)
                } else {
                    polling
                };
                thread::park_timeout(Duration::from_millis(delay));
            }
        });
        threads.push(handle.thread().clone());
        handles.push(handle);
    }
    WorkerGroup {
        stop,
        handles,
        threads,
    }
}

fn evaluate(
    algo: &Algo,
    state: &mut AlgoState,
    sensors: &SensorStore,
    devices: &DeviceStore,
    dry_run: bool,
    algo_id: u32,
    report: bool,
) -> Result<EvalResult, String> {
    let temp = sensors.read(&algo.sensor)?;
    let count = algo.count();
    let level = match algo.kind {
        AlgoKind::Monitor | AlgoKind::Sic => monitor_level(algo, &mut state.levels, temp, count),
        AlgoKind::Ss => threshold_level(algo, temp, count),
        AlgoKind::Chrono => {
            return Err(format!("{:?} is not implemented yet", algo.kind));
        }
    };

    let (last_temp, last2_temp) = if matches!(algo.kind, AlgoKind::Sic) {
        sensors.history(&algo.sensor)
    } else {
        (0, 0)
    };

    let (sic_max, sic_min, usb_online) = if matches!(algo.kind, AlgoKind::Sic) {
        let charger_temp = devices
            .read_sysfs_i64("sys/class/thermal/thermal_message/charger_temp")
            .filter(|value| *value < 9)
            .unwrap_or(0);
        let screen_state = devices
            .read_sysfs_i64("sys/class/thermal/thermal_message/screen_state")
            .unwrap_or(-22);
        let offset = if screen_state != 0 {
            algo.proportion * charger_temp * 1000 / 100
        } else {
            charger_temp * 1000
        };
        let (max, min) = sic_limits(algo, algo.devices.len(), offset);
        let usb_online = devices
            .read_sysfs_i64("sys/class/thermal/thermal_message/usb_online")
            .unwrap_or(-22);
        (max, min, usb_online)
    } else {
        (Vec::new(), Vec::new(), 0)
    };
    let cpu_nolimit = if matches!(algo.kind, AlgoKind::Ss) {
        devices
            .read_sysfs_i64("sys/class/thermal/thermal_message/cpu_nolimit_temp")
            .unwrap_or(0)
    } else {
        i64::MIN
    };

    let mut writes = if report {
        Vec::with_capacity(algo.devices.len())
    } else {
        Vec::new()
    };
    for (index, device) in algo.devices.iter().enumerate() {
        let use_default = level < 0
            || (matches!(algo.kind, AlgoKind::Ss)
                && algo.trig.get(level as usize).copied().unwrap_or(i64::MAX) <= cpu_nolimit);
        let target = if use_default {
            devices.default_target(device)
        } else {
            let row = algo.targets.get(level as usize);
            row.and_then(|row| row.get(index)).copied().unwrap_or(0)
        };

        let target = match algo.kind {
            AlgoKind::Ss => {
                let levels = devices.levels(device);
                ss_target(
                    target,
                    devices.default_target(device),
                    levels.as_deref().unwrap_or(&[]),
                    &mut state.last_level,
                )
            }
            AlgoKind::Monitor => {
                if target == 0 {
                    devices
                        .levels(device)
                        .as_ref()
                        .and_then(|levels| levels.first().copied())
                        .unwrap_or(0)
                } else {
                    target
                }
            }
            _ => target,
        };

        let target = if matches!(algo.kind, AlgoKind::Sic) {
            if level < 0 || usb_online == 0 {
                devices.default_target(device)
            } else {
                let current = devices.read_target(device);
                let ks = algo.ks.get(level as usize).copied().unwrap_or(0);
                let ki = algo.ki.get(level as usize).copied().unwrap_or(0);
                let kc = algo.kc.get(level as usize).copied().unwrap_or(0);
                let max = sic_max.get(level as usize).copied().unwrap_or(0) * 1000;
                let min = sic_min.get(level as usize).copied().unwrap_or(0) * 1000;
                sic_output(
                    target, temp, last_temp, last2_temp, ks, ki, kc, current, max, min,
                )
            }
        } else {
            target
        };

        let requester = algo_id.saturating_mul(1024).saturating_add(index as u32);
        match devices.set_target_for(requester, device, target, dry_run) {
            Ok(effective) => {
                if report {
                    writes.push((device.clone(), effective));
                }
            }
            Err(error) => eprintln!("{}: {error}", algo.name),
        }
    }

    Ok(EvalResult {
        temp,
        level,
        writes,
    })
}

fn sic_limits(algo: &Algo, device_count: usize, offset: i64) -> (Vec<i64>, Vec<i64>) {
    let groups = algo.targets.len();
    let mut out_max = vec![0; groups];
    let mut out_min = vec![0; groups];
    for group in 0..groups {
        out_max[group] = algo.max.get(group).copied().unwrap_or(0);
        out_min[group] = algo.min.get(group).copied().unwrap_or(0);
    }

    let mut first_target = vec![0; device_count];
    let mut first_max = vec![0; device_count];
    for device in 0..device_count {
        for group in 0..groups {
            let target = algo
                .targets
                .get(group)
                .and_then(|row| row.get(device))
                .copied()
                .unwrap_or(0);
            if target != 0 {
                first_target[device] = target;
                first_max[device] = algo.max.get(group).copied().unwrap_or(0);
                break;
            }
        }
    }

    for group in 1..groups {
        for device in 0..device_count {
            let target = algo
                .targets
                .get(group)
                .and_then(|row| row.get(device))
                .copied()
                .unwrap_or(0);
            if target == 0 {
                continue;
            }
            let limit = first_target[device] + offset;
            if target < limit {
                out_max[group] = out_max[group - 1];
                out_min[group] = out_min[group - 1];
            } else {
                let previous = algo
                    .targets
                    .get(group - 1)
                    .and_then(|row| row.get(device))
                    .copied()
                    .unwrap_or(0);
                if previous != 0 && previous < limit {
                    out_max[group] = first_max[device];
                }
            }
        }
    }

    (out_max, out_min)
}

#[allow(clippy::too_many_arguments)]
fn sic_output(
    target: i64,
    temp: i64,
    last_temp: i64,
    last2_temp: i64,
    ks: i64,
    ki: i64,
    kc: i64,
    current: i64,
    max: i64,
    min: i64,
) -> i64 {
    let ek = target - temp;
    let ek1 = target - last_temp;
    let ek2 = target - last2_temp;
    let pid = (ki * ek + ks * (ek - ek1) + kc * (ek - 2 * ek1 + ek2)) / 1000;
    let mut output = current + pid;
    if output >= max {
        output = max;
    }
    if output <= min {
        output = min;
    }
    output
}

fn monitor_level(algo: &Algo, state: &mut [bool], temp: i64, count: usize) -> i64 {
    if algo.reverse {
        for (index, active) in state.iter_mut().enumerate().take(count) {
            if temp <= algo.trig[index] {
                *active = true;
            }
            if temp >= algo.clr[index] {
                *active = false;
            }
        }
    } else {
        for index in (0..count).rev() {
            if temp >= algo.trig[index] {
                state[index] = true;
            }
            if temp <= algo.clr[index] {
                state[index] = false;
            }
        }
    }

    if algo.reverse {
        state
            .iter()
            .rposition(|active| !active)
            .map(|index| index as i64 + 1)
            .unwrap_or(0)
    } else {
        state
            .iter()
            .position(|active| !active)
            .map(|index| index as i64 - 1)
            .unwrap_or(count as i64 - 1)
    }
}

fn ss_target(target: i64, default: i64, levels: &[i64], last: &mut i64) -> i64 {
    if levels.is_empty() {
        return target;
    }
    let count = levels.len() as i64;
    let target = if target == 0 { levels[0] } else { target };
    let mut index = *last;
    if (0..count).contains(&index) {
        let level = levels[index as usize];
        if target < level {
            index += 1;
        } else if target > level {
            index -= 1;
        }
        *last = index;
    }
    if index < -1 {
        index = -1;
        *last = -1;
    }
    if index >= count {
        index = count - 1;
        *last = index;
    }
    if index < 0 {
        *last = 0;
        default
    } else {
        levels[index as usize]
    }
}

fn threshold_level(algo: &Algo, temp: i64, count: usize) -> i64 {
    if count == 0 {
        return -1;
    }
    if algo.reverse {
        algo.trig
            .iter()
            .position(|trigger| temp <= *trigger)
            .map(|index| index as i64)
            .unwrap_or(-1)
    } else {
        algo.trig
            .iter()
            .rposition(|trigger| *trigger <= temp)
            .map(|index| index as i64)
            .unwrap_or(-1)
    }
}

fn parse_targets(value: Option<&str>) -> Vec<Vec<i64>> {
    let Some(value) = value else {
        return Vec::new();
    };
    value
        .split_whitespace()
        .map(|row| {
            row.split('+')
                .filter_map(|part| part.trim().parse::<i64>().ok())
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_thermal;

    #[test]
    fn monitor_hysteresis() {
        let config = parse_thermal(
            "[M]\nalgo_type monitor\nsensor S\ndevice d\ntrig 10 20\nclr 8 18\ntarget 1 2\n",
        );
        let algo = Algo::from_config(&config).pop().unwrap();
        let mut state = vec![false; algo.count()];
        assert_eq!(monitor_level(&algo, &mut state, 9, 2), -1);
        assert_eq!(monitor_level(&algo, &mut state, 15, 2), 0);
        assert_eq!(monitor_level(&algo, &mut state, 25, 2), 1);
        assert_eq!(monitor_level(&algo, &mut state, 19, 2), 1);
        assert_eq!(monitor_level(&algo, &mut state, 17, 2), 0);
    }

    #[test]
    fn ss_level_ramps_one_step() {
        let levels = [2_572_800, 2_188_800, 1_920_000];
        let mut last = 0;
        assert_eq!(ss_target(1_920_000, 0, &levels, &mut last), 2_188_800);
        assert_eq!(last, 1);
        assert_eq!(ss_target(1_920_000, 0, &levels, &mut last), 1_920_000);
        assert_eq!(last, 2);
        last = 0;
        assert_eq!(ss_target(0, 0, &levels, &mut last), 2_572_800);
        last = 0;
        assert_eq!(ss_target(3_000_000, 42, &levels, &mut last), 42);
    }

    #[test]
    fn monitor_reverse_levels() {
        let config = parse_thermal(
            "[M]\nalgo_type monitor\nsensor S\ndevice d\ntrig 1 3\nclr 2 4\ntarget 10 20\nreverse 1\n",
        );
        let algo = Algo::from_config(&config).pop().unwrap();
        let mut state = vec![false; algo.count()];
        assert_eq!(monitor_level(&algo, &mut state, 0, 2), 0);
        assert_eq!(monitor_level(&algo, &mut state, 3, 2), 1);
        assert_eq!(monitor_level(&algo, &mut state, 2, 2), 1);
        assert_eq!(monitor_level(&algo, &mut state, 4, 2), 2);
        assert_eq!(monitor_level(&algo, &mut state, 5, 2), 2);
    }

    #[test]
    fn ss_threshold() {
        let config =
            parse_thermal("[S]\nalgo_type ss\nsensor S\ndevice d\ntrig 10 20 30\ntarget 1 2 3\n");
        let algo = Algo::from_config(&config).pop().unwrap();
        assert_eq!(threshold_level(&algo, 5, 3), -1);
        assert_eq!(threshold_level(&algo, 10, 3), 0);
        assert_eq!(threshold_level(&algo, 25, 3), 1);
        assert_eq!(threshold_level(&algo, 35, 3), 2);
    }

    #[test]
    fn target_rows_split_plus() {
        assert_eq!(parse_targets(Some("1+2 3+4")), vec![vec![1, 2], vec![3, 4]]);
    }

    #[test]
    fn sic_pid_clamps_to_limits() {
        assert_eq!(
            sic_output(20000, 25000, 24000, 23000, 0, 1000, 0, 1000, 2000, 1000),
            1000
        );
        assert_eq!(
            sic_output(30000, 25000, 24000, 23000, 0, 1000, 0, 1000, 10000, 0),
            6000
        );
    }

    #[test]
    fn sic_limits_follow_previous_group_below_offset() {
        let config = parse_thermal(
            "[S]\nalgo_type sic\nsensor s\ndevice d\ntrig 10 20 30 40\nclr 5 15 25 35\ntarget 0 1000 500 2000\nmax 10 20 30 40\nmin 1 2 3 4\n",
        );
        let algo = Algo::from_config(&config).pop().unwrap();
        let (max, min) = sic_limits(&algo, 1, 0);
        assert_eq!(max, vec![10, 20, 20, 20]);
        assert_eq!(min, vec![1, 2, 2, 4]);
    }
}

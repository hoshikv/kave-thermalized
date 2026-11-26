use crate::sysfs::{find_zone_by_type, RawCache, SysfsNode};
use std::sync::Arc;
use std::time::{Duration, Instant};

const REPROBE_INTERVAL: Duration = Duration::from_secs(5);

pub struct SensorSet {
    cache: Arc<RawCache>,
    cpu_therm: Option<SysfsNode>,
    battery: Option<SysfsNode>,
    charger0: Option<SysfsNode>,
    wifi: Option<SysfsNode>,
    pa0: Option<SysfsNode>,
    pa1: Option<SysfsNode>,
    quiet: Option<SysfsNode>,
    conn: Option<SysfsNode>,
    batt_temp_node: Option<SysfsNode>,
    last_reprobe: Option<Instant>,
}

pub struct Reading {
    pub virtual_temp: i32,
    pub battery_temp: i32,
}

pub struct Weights {
    pub cpu: i32,
    pub batt: i32,
    pub chg: i32,
    pub wifi: i32,
    pub pa0: i32,
    pub pa1: i32,
    pub quiet: i32,
    pub weight_sum: i32,
    pub compensation: i32,
}

impl Default for Weights {
    fn default() -> Self {
        // Matches stock peridot VIRTUAL-SENSOR0 in thermal-normal.conf.
        Self {
            cpu: -26,
            batt: 301,
            chg: 147,
            wifi: 330,
            pa0: 36,
            pa1: 119,
            quiet: -2,
            weight_sum: 1000,
            compensation: 2015,
        }
    }
}

impl SensorSet {
    pub fn probe(cache: Arc<RawCache>) -> Self {
        let open_zone = |name: &str| -> Option<SysfsNode> {
            find_zone_by_type(name).and_then(|p| SysfsNode::open(&p))
        };
        Self {
            cache,
            cpu_therm: open_zone("cpu_therm"),
            battery: open_zone("battery"),
            charger0: open_zone("charger_therm0"),
            wifi: open_zone("wifi_therm"),
            pa0: open_zone("pa_therm0"),
            pa1: open_zone("pa_therm1"),
            quiet: open_zone("quiet_therm"),
            conn: open_zone("conn_therm"),
            batt_temp_node: SysfsNode::open("/sys/class/power_supply/battery/temp"),
            last_reprobe: None,
        }
    }

    pub fn all_present(&self) -> bool {
        self.cpu_therm.is_some()
            && self.battery.is_some()
            && self.charger0.is_some()
            && self.wifi.is_some()
            && self.pa0.is_some()
            && self.pa1.is_some()
            && self.quiet.is_some()
            && self.batt_temp_node.is_some()
    }

    fn reprobe_missing(&mut self) {
        if self.all_present() && self.conn.is_some() {
            return;
        }
        if let Some(last) = self.last_reprobe {
            if last.elapsed() < REPROBE_INTERVAL {
                return;
            }
        }
        self.last_reprobe = Some(Instant::now());
        let open_zone = |name: &str| -> Option<SysfsNode> {
            find_zone_by_type(name).and_then(|p| SysfsNode::open(&p))
        };
        if self.cpu_therm.is_none() {
            self.cpu_therm = open_zone("cpu_therm");
        }
        if self.battery.is_none() {
            self.battery = open_zone("battery");
            if self.battery.is_some() {
                crate::klog("late-probed thermal zone: battery");
            }
        }
        if self.charger0.is_none() {
            self.charger0 = open_zone("charger_therm0");
        }
        if self.wifi.is_none() {
            self.wifi = open_zone("wifi_therm");
        }
        if self.pa0.is_none() {
            self.pa0 = open_zone("pa_therm0");
        }
        if self.pa1.is_none() {
            self.pa1 = open_zone("pa_therm1");
        }
        if self.quiet.is_none() {
            self.quiet = open_zone("quiet_therm");
        }
        if self.conn.is_none() {
            self.conn = open_zone("conn_therm");
        }
        if self.batt_temp_node.is_none() {
            self.batt_temp_node = SysfsNode::open("/sys/class/power_supply/battery/temp");
            if self.batt_temp_node.is_some() {
                crate::klog("late-probed power_supply/battery/temp");
            }
        }
    }

    fn read_opt(node: &mut Option<SysfsNode>, cache: &RawCache) -> Option<i32> {
        node.as_mut().and_then(|n| n.read_i32_cached(Some(cache)))
    }

    fn read(node: &mut Option<SysfsNode>, fallback: i32, cache: &RawCache) -> i32 {
        Self::read_opt(node, cache).unwrap_or(fallback)
    }

    pub fn read_all(&mut self, w: &Weights) -> Reading {
        self.reprobe_missing();
        let cache = Arc::clone(&self.cache);
        let t_cpu = Self::read(&mut self.cpu_therm, 25000, &cache);
        let batt_ps_mc = Self::read_opt(&mut self.batt_temp_node, &cache).map(|v| {
            // power_supply/battery/temp is typically in tenths of °C (e.g. 419 -> 41900 m°C).
            if v > -1000 && v <= 1000 {
                v * 100
            } else {
                v
            }
        });
        let t_batt = Self::read_opt(&mut self.battery, &cache)
            .or(batt_ps_mc)
            .unwrap_or(25000);
        let t_chg = Self::read(&mut self.charger0, 25000, &cache);
        let t_wifi = Self::read(&mut self.wifi, 25000, &cache);
        let t_pa0 = Self::read(&mut self.pa0, 25000, &cache);
        let t_pa1 = Self::read(&mut self.pa1, 25000, &cache);
        let t_quiet = Self::read(&mut self.quiet, 25000, &cache);
        let divisor = if w.weight_sum == 0 { 1 } else { w.weight_sum };
        let weighted_sum = i64::from(w.cpu) * i64::from(t_cpu)
            + i64::from(w.batt) * i64::from(t_batt)
            + i64::from(w.chg) * i64::from(t_chg)
            + i64::from(w.wifi) * i64::from(t_wifi)
            + i64::from(w.pa0) * i64::from(t_pa0)
            + i64::from(w.pa1) * i64::from(t_pa1)
            + i64::from(w.quiet) * i64::from(t_quiet);
        let virtual_temp = (weighted_sum / i64::from(divisor)) as i32 + w.compensation;

        let battery_temp = batt_ps_mc.unwrap_or(t_batt);

        Reading {
            virtual_temp,
            battery_temp,
        }
    }

    pub fn log_missing(&mut self) {
        if !self.all_present() {
            crate::klog("warning: some thermal zones missing, using fallback values");
        }
        let _ = self
            .conn
            .as_mut()
            .and_then(|n| n.read_i32_cached(Some(&self.cache)));
    }
}

use crate::sysfs::{find_zone_by_type, RawCache, SysfsNode};
use std::sync::Arc;

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
    hotspot: Vec<Option<SysfsNode>>,
    batt_temp_node: Option<SysfsNode>,
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
    pub conn: i32,
    pub hotspot: i32,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            cpu: 200,
            batt: 200,
            chg: 100,
            wifi: 60,
            pa0: 40,
            pa1: 80,
            quiet: 150,
            conn: 100,
            hotspot: 60,
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
            hotspot: [
                "aoss-0".to_string(),
                "cpuss-0".to_string(),
                "cpuss-1".to_string(),
                "ddr".to_string(),
                "video".to_string(),
                "camera-0".to_string(),
                "camera-1".to_string(),
                "nsphvx-0".to_string(),
                "nsphmx-0".to_string(),
            ]
            .iter()
            .map(|n| open_zone(n))
            .collect(),
            batt_temp_node: SysfsNode::open("/sys/class/power_supply/battery/temp"),
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
    }

    fn read(node: &mut Option<SysfsNode>, fallback: i32, cache: &RawCache) -> i32 {
        node.as_mut()
            .and_then(|n| n.read_i32_cached(Some(cache)))
            .unwrap_or(fallback)
    }

    pub fn read_all(&mut self, w: &Weights) -> Reading {
        let cache = Arc::clone(&self.cache);
        let t_cpu = Self::read(&mut self.cpu_therm, 25000, &cache);
        let t_batt = Self::read(&mut self.battery, 25000, &cache);
        let t_chg = Self::read(&mut self.charger0, 25000, &cache);
        let t_wifi = Self::read(&mut self.wifi, 25000, &cache);
        let t_pa0 = Self::read(&mut self.pa0, 25000, &cache);
        let t_pa1 = Self::read(&mut self.pa1, 25000, &cache);
        let t_quiet = Self::read(&mut self.quiet, 25000, &cache);
        let t_conn = Self::read(&mut self.conn, 25000, &cache);

        let mut t_hotspot = 25000;
        for node in self.hotspot.iter_mut() {
            if let Some(value) = node.as_mut().and_then(|n| n.read_i32_cached(Some(&cache))) {
                if value > t_hotspot {
                    t_hotspot = value;
                }
            }
        }

        let sum = w.cpu + w.batt + w.chg + w.wifi + w.pa0 + w.pa1 + w.quiet + w.conn + w.hotspot;

        let virtual_temp = if sum > 0 {
            (w.cpu * t_cpu
                + w.batt * t_batt
                + w.chg * t_chg
                + w.wifi * t_wifi
                + w.pa0 * t_pa0
                + w.pa1 * t_pa1
                + w.quiet * t_quiet
                + w.conn * t_conn
                + w.hotspot * t_hotspot)
                / sum
        } else {
            t_quiet
        };

        let battery_ps = Self::read(&mut self.batt_temp_node, 25000, &cache);
        let battery_temp = if battery_ps > 1000 {
            battery_ps
        } else {
            t_batt
        };

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

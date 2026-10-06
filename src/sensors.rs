use crate::sysfs::{find_zone_by_type, SysfsNode};

pub struct SensorSet {
    cpu_therm: Option<SysfsNode>,
    battery: Option<SysfsNode>,
    charger0: Option<SysfsNode>,
    wifi: Option<SysfsNode>,
    pa0: Option<SysfsNode>,
    pa1: Option<SysfsNode>,
    quiet: Option<SysfsNode>,
    flash: Option<SysfsNode>,
    conn: Option<SysfsNode>,
    gpu_therms: Vec<Option<SysfsNode>>,
    cluster0: Vec<Option<SysfsNode>>,
    cluster1: Vec<Option<SysfsNode>>,
    cluster2: Vec<Option<SysfsNode>>,
    mdmss: Vec<Option<SysfsNode>>,
    hotspot: Vec<Option<SysfsNode>>,
    pub soc: Option<SysfsNode>,
    pub batt_temp_node: Option<SysfsNode>,
    pub screen: Option<SysfsNode>,
}

pub struct Reading {
    pub virtual_temp: i32,
    pub battery_temp: i32,
    pub flash_temp: i32,
    pub gpu_therm: i32,
    pub soc: i32,
    pub screen_on: bool,
    pub cpu_therm: i32,
    pub cpu0_junction: i32,
    pub cpu3_junction: i32,
    pub cpu7_junction: i32,
    pub modem_temp: i32,
    pub hotspot_temp: i32,
    pub charger_therm: i32,
    pub wifi_therm: i32,
    pub pa0_temp: i32,
    pub pa1_temp: i32,
    pub quiet_temp: i32,
    pub conn_temp: i32,
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
    pub fn probe() -> Self {
        let open_zone = |name: &str| -> Option<SysfsNode> {
            find_zone_by_type(name).and_then(|p| SysfsNode::open(&p, false))
        };
        Self {
            cpu_therm: open_zone("cpu_therm"),
            battery: open_zone("battery"),
            charger0: open_zone("charger_therm0"),
            wifi: open_zone("wifi_therm"),
            pa0: open_zone("pa_therm0"),
            pa1: open_zone("pa_therm1"),
            quiet: open_zone("quiet_therm"),
            flash: open_zone("flash_therm"),
            conn: open_zone("conn_therm"),
            gpu_therms: (0..4).map(|i| open_zone(&format!("gpuss-{}", i))).collect(),
            cluster0: (0..3)
                .flat_map(|c| (0..2).map(move |s| open_zone(&format!("cpu-0-{}-{}", c, s))))
                .collect(),
            cluster1: (0..4)
                .flat_map(|c| (0..2).map(move |s| open_zone(&format!("cpu-1-{}-{}", c, s))))
                .collect(),
            cluster2: (0..2)
                .flat_map(|c| (0..2).map(move |s| open_zone(&format!("cpu-2-{}-{}", c, s))))
                .collect(),
            mdmss: (0..4).map(|i| open_zone(&format!("mdmss-{}", i))).collect(),
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
            soc: SysfsNode::open("/sys/class/power_supply/battery/capacity", false),
            batt_temp_node: SysfsNode::open("/sys/class/power_supply/battery/temp", false),
            screen: SysfsNode::open("/sys/class/thermal/thermal_message/screen_state", false),
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

    fn read(node: &mut Option<SysfsNode>, fallback: i32) -> i32 {
        node.as_mut().and_then(|n| n.read_i32()).unwrap_or(fallback)
    }

    fn opt_read(node: &mut Option<SysfsNode>) -> Option<i32> {
        node.as_mut().and_then(|n| n.read_i32())
    }

    pub fn read_all(&mut self, w: &Weights) -> Reading {
        let t_cpu = Self::read(&mut self.cpu_therm, 25000);
        let t_batt = Self::read(&mut self.battery, 25000);
        let t_chg = Self::read(&mut self.charger0, 25000);
        let t_wifi = Self::read(&mut self.wifi, 25000);
        let t_pa0 = Self::read(&mut self.pa0, 25000);
        let t_pa1 = Self::read(&mut self.pa1, 25000);
        let t_quiet = Self::read(&mut self.quiet, 25000);
        let t_conn = Self::read(&mut self.conn, 25000);

        let mut t_gpu = 25000;
        for n in self.gpu_therms.iter_mut() {
            if let Some(node) = n {
                if let Some(v) = node.read_i32() {
                    if v > t_gpu {
                        t_gpu = v;
                    }
                }
            }
        }

        let sum = w.cpu + w.batt + w.chg + w.wifi + w.pa0 + w.pa1 + w.quiet + w.conn + w.hotspot;

        let mut t_c0 = 25000;
        for n in self.cluster0.iter_mut() {
            if let Some(v) = Self::opt_read(n) {
                if v > t_c0 {
                    t_c0 = v;
                }
            }
        }
        let mut t_c1 = 25000;
        for n in self.cluster1.iter_mut() {
            if let Some(v) = Self::opt_read(n) {
                if v > t_c1 {
                    t_c1 = v;
                }
            }
        }
        let mut t_c2 = 25000;
        for n in self.cluster2.iter_mut() {
            if let Some(v) = Self::opt_read(n) {
                if v > t_c2 {
                    t_c2 = v;
                }
            }
        }
        let mut t_modem = 25000;
        for n in self.mdmss.iter_mut() {
            if let Some(v) = Self::opt_read(n) {
                if v > t_modem {
                    t_modem = v;
                }
            }
        }
        let mut t_hotspot = 25000;
        for n in self.hotspot.iter_mut() {
            if let Some(v) = Self::opt_read(n) {
                if v > t_hotspot {
                    t_hotspot = v;
                }
            }
        }

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

        let battery_ps = Self::read(&mut self.batt_temp_node, 25000);
        let battery_temp = if battery_ps > 1000 { battery_ps } else { t_batt };
        let flash_temp = Self::read(&mut self.flash, 25000);
        let soc = Self::read(&mut self.soc, 50);
        let scr = Self::read(&mut self.screen, 0);

        Reading {
            virtual_temp,
            battery_temp,
            flash_temp,
            gpu_therm: t_gpu,
            soc,
            screen_on: scr == 1,
            cpu_therm: t_cpu,
            cpu0_junction: t_c0,
            cpu3_junction: t_c1,
            cpu7_junction: t_c2,
            modem_temp: t_modem,
            hotspot_temp: t_hotspot,
            charger_therm: t_chg,
            wifi_therm: t_wifi,
            pa0_temp: t_pa0,
            pa1_temp: t_pa1,
            quiet_temp: t_quiet,
            conn_temp: t_conn,
        }
    }

    pub fn log_missing(&mut self) {
        if !self.all_present() {
            crate::klog("warning: some thermal zones missing, using fallback values");
        }
        let _ = self.conn.as_mut().map(|n| n.read_i32());
    }
}

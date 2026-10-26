use crate::config::{PowerCfg, PropCfg};
use crate::sysfs::SysfsNode;
use std::process::Command;

fn snap_to_freq_grid(policy: u32, want: i32) -> i32 {
    if want <= 0 {
        return want;
    }
    let path = format!(
        "/sys/devices/system/cpu/cpufreq/policy{}/scaling_available_frequencies",
        policy
    );
    let mut node = match SysfsNode::open(&path, false) {
        Some(n) => n,
        None => return want,
    };
    let list = match node.read_string() {
        Some(s) => s,
        None => return want,
    };
    let mut best = 0i32;
    for tok in list.split_whitespace() {
        if let Ok(v) = tok.parse::<i32>() {
            if v <= want && v > best {
                best = v;
            }
        }
    }
    if best > 0 && best != want {
        crate::klog(&format!(
            "power: policy{} want {} off-grid -> snapped {}",
            policy, want, best
        ));
        return best;
    }
    if best == 0 {
        crate::klog(&format!(
            "power: policy{} want {} no grid value <= it, keeping",
            policy, want
        ));
    }
    want
}

pub struct CpuLimit {
    node: Option<SysfsNode>,
    min_node: Option<SysfsNode>,
    def_min: i32,
    fab_min: i32,
    boost_min: i32,
    lvl0: i32,
    baseline: i32,
    cur: i32,
    min_restored: i32,
    index: usize,
}

pub struct Tuner {
    policies: Vec<CpuLimit>,
    governor: Option<Governor>,
    swappiness: Option<SysfsNode>,
    swappiness_target: Option<i32>,
    enforce_every: u64,
    gate_temp: i32,
    gated: bool,
    props: Vec<PropCfg>,
    props_applied: bool,
    boost_node: Option<SysfsNode>,
    boost_want: i32,
    dirty_ratio_node: Option<SysfsNode>,
    dirty_ratio_want: Option<i32>,
    dirty_bg_node: Option<SysfsNode>,
    dirty_bg_want: Option<i32>,
    boot_guard_s: u64,
    boot_maxes: Vec<i32>,
    boot_active: bool,
    boot_logged: bool,
}

struct Governor {
    desired: &'static str,
    active: bool,
}

impl Tuner {
    pub fn new(cfg: &PowerCfg, baseline: i32) -> Self {
        let mut policies = Vec::new();
        for (idx, lvl0) in [
            (0, cfg.max_freq_p0),
            (3, cfg.max_freq_p3),
            (6, cfg.max_freq_p6),
            (7, cfg.max_freq_p7),
        ] {
            let base = snap_to_freq_grid(idx as u32, lvl0.unwrap_or(0));
            let mut node = if base > 0 {
                SysfsNode::open(
                    &format!(
                        "/sys/devices/system/cpu/cpufreq/policy{}/scaling_max_freq",
                        idx
                    ),
                    true,
                )
            } else {
                None
            };
            let cur = node.as_mut().and_then(|n| n.read_i32()).unwrap_or(base);
            let bmax = SysfsNode::open(
                &format!(
                    "/sys/devices/system/cpu/cpufreq/policy{}/cpuinfo_max_freq",
                    idx
                ),
                false,
            )
            .and_then(|mut n| n.read_i32())
            .unwrap_or(base);
            let baseline = if base > 0 && base < bmax { base } else { bmax };
            let mut min_node = SysfsNode::open(
                &format!(
                    "/sys/devices/system/cpu/cpufreq/policy{}/scaling_min_freq",
                    idx
                ),
                true,
            );
            let def_min = min_node
                .as_mut()
                .and_then(|n| n.read_i32())
                .unwrap_or(0);
            let fab_min = SysfsNode::open(
                &format!(
                    "/sys/devices/system/cpu/cpufreq/policy{}/cpuinfo_min_freq",
                    idx
                ),
                false,
            )
            .and_then(|mut n| n.read_i32())
            .unwrap_or(def_min);
            let boost_min = match idx {
                0 => 1478400,
                3 => 1824000,
                6 => 2150400,
                _ => 1651200,
            };
            policies.push(CpuLimit {
                node,
                min_node,
                def_min,
                fab_min,
                boost_min,
                lvl0: base,
                baseline,
                cur,
                min_restored: 0,
                index: idx,
            });
        }
        let _ = baseline;

        let governor = match cfg.governor.as_deref() {
            Some("schedutil") => Some(Governor { desired: "schedutil", active: false }),
            Some("conservative") => Some(Governor { desired: "conservative", active: false }),
            Some("walt") => Some(Governor { desired: "walt", active: false }),
            Some("powersave") => Some(Governor { desired: "powersave", active: false }),
            Some("performance") => Some(Governor { desired: "performance", active: false }),
            _ => None,
        };

        let swappiness = SysfsNode::open("/proc/sys/vm/swappiness", true);

        let boost_node = SysfsNode::open("/sys/devices/system/cpu/cpufreq/boost", true);
        let boost_want = cfg.boost.unwrap_or(0);
        let dirty_ratio_node = SysfsNode::open("/proc/sys/vm/dirty_ratio", true);
        let dirty_bg_node = SysfsNode::open("/proc/sys/vm/dirty_background_ratio", true);

        Self {
            policies,
            governor,
            swappiness,
            swappiness_target: Some(cfg.swappiness.unwrap_or(100)),
            enforce_every: 10,
            gate_temp: cfg.gate_temp.unwrap_or(48000),
            gated: false,
            props: cfg.props.clone().unwrap_or_default(),
            props_applied: false,
            boost_node,
            boost_want,
            dirty_ratio_node,
            dirty_ratio_want: cfg.dirty_ratio,
            dirty_bg_node,
            dirty_bg_want: cfg.dirty_bg,
            boot_guard_s: cfg.boot_guard_s.unwrap_or(0),
            boot_maxes: vec![
                snap_to_freq_grid(0, cfg.boot_max_p0.unwrap_or(0)),
                snap_to_freq_grid(3, cfg.boot_max_p3.unwrap_or(0)),
                snap_to_freq_grid(6, cfg.boot_max_p6.unwrap_or(0)),
                snap_to_freq_grid(7, cfg.boot_max_p7.unwrap_or(0)),
            ],
            boot_active: false,
            boot_logged: false,
        }
    }

    fn boot_active_check(&mut self, elapsed_secs: u64) -> bool {
        let active = self.boot_guard_s > 0 && elapsed_secs < self.boot_guard_s;
        if active && !self.boot_logged {
            self.boot_logged = true;
            crate::klog(&format!(
                "power: boot guard active {}s freq[{}]",
                self.boot_guard_s,
                self.boot_maxes
                    .iter()
                    .filter(|&&v| v > 0)
                    .map(|v| format!("{}k", v / 1000))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        if !active && self.boot_active {
            crate::klog("power: boot guard finished, releasing freq caps");
        }
        self.boot_active = active;
        active
    }

    pub fn apply(&mut self, elapsed_secs: u64, virtual_temp: i32) {
        self.boot_active_check(elapsed_secs);
        if elapsed_secs % self.enforce_every != 0 {
            return;
        }

        if !self.props_applied {
            let mut changed = Vec::new();
            for p in &self.props {
                let Ok(out) = Command::new("/system/bin/getprop").arg(&p.name).output() else {
                    continue;
                };
                let cur = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if cur != p.value {
                    let _ = Command::new("/system/bin/setprop")
                        .arg(&p.name)
                        .arg(&p.value)
                        .status();
                    changed.push(format!("{}={}", p.name, p.value));
                }
            }
            if !changed.is_empty() {
                crate::klog(&format!("power: props {}", changed.join(" ")));
            }
            self.props_applied = true;
        }

        let gated_now = virtual_temp >= self.gate_temp;
        if gated_now && !self.gated {
            self.gated = true;
            crate::klog(&format!(
                "power: freq caps ON (gate {})",
                self.gate_temp
            ));
        } else if !gated_now && self.gated && virtual_temp < self.gate_temp - 3000 {
            self.gated = false;
            crate::klog("power: freq caps OFF");
            for p in self.policies.iter_mut() {
                if let Some(n) = p.node.as_mut() {
                    if p.lvl0 > 0 && p.cur != p.baseline {
                        let _ = n.write_i32(p.baseline).then(|| {
                            p.cur = p.baseline;
                        });
                    }
                }
            }
        }

        for p in self.policies.iter_mut() {
            if p.lvl0 <= 0 {
                continue;
            }
            let desired = if self.gated {
                p.lvl0
            } else {
                p.baseline
            };
            let boot_cap = self.boot_maxes.get(p.index).copied().unwrap_or(0);
            let desired = if self.boot_active && boot_cap > 0 {
                desired.min(boot_cap)
            } else {
                desired
            };
            if let Some(n) = p.node.as_mut() {
                if let Some(cur) = n.read_i32() {
                    if cur != desired {
                        if n.write_i32(desired) {
                            p.cur = desired;
                        }
                    }
                }
            }
            if let Some(n) = p.min_node.as_mut() {
                if p.min_restored == 0 {
                    let cur = n.read_i32().unwrap_or(-1);
                    if cur > p.fab_min {
                        let _ = n.write_i32(p.fab_min);
                        crate::klog(&format!(
                            "power: min_freq restore -> {}",
                            p.fab_min
                        ));
                    }
                    p.min_restored = 1;
                }
            }
        }

        if let Some(g) = self.governor.as_mut() {
            if !g.active {
                let mut tried = 0usize;
                let mut ok = 0usize;
                for idx in [0usize, 3, 6, 7] {
                    let path = format!(
                        "/sys/devices/system/cpu/cpufreq/policy{}/scaling_governor",
                        idx
                    );
                    if let Some(mut n) = SysfsNode::open(&path, true) {
                        tried += 1;
                        if n.write_raw(g.desired) {
                            ok += 1;
                        }
                    }
                }
                if tried > 0 && ok == tried {
                    g.active = true;
                    crate::klog(&format!("power: governor -> {}", g.desired));
                }
            }
            if g.desired == "walt" {
                for idx in [0usize, 3, 7] {
                    for (name, val) in [("up_rate_limit_us", "10000"), ("down_rate_limit_us", "30000")] {
                        let path = format!(
                            "/sys/devices/system/cpu/cpufreq/policy{}/walt/{}",
                            idx, name
                        );
                        if let Some(mut n) = SysfsNode::open(&path, true) {
                            if n.read_string().as_deref() != Some(val) && n.write_raw(val) {
                                crate::klog(&format!("power: walt {}/policy{} -> {}", name, idx, val));
                            }
                        }
                    }
                }
            }
        }

        if let Some(target) = self.swappiness_target {
            if let Some(n) = self.swappiness.as_mut() {
                if let Some(cur) = n.read_i32() {
                    if cur != target && n.write_i32(target) {
                        crate::klog(&format!("power: swappiness {} -> {}", cur, target));
                    }
                } else if n.write_i32(target) {
                    crate::klog(&format!("power: swappiness -> {}", target));
                }
            }
        }

        if let Some(n) = self.boost_node.as_mut() {
            if self.boost_want == 0 {
                if let Some(cur) = n.read_string() {
                    if cur.trim() != "0" && n.write_raw("0") {
                        crate::klog("power: cpufreq boost -> 0");
                    }
                } else if n.read_i32().unwrap_or(-1) != 0 {
                    let _ = n.write_i32(0);
                }
            }
        }

        if let Some(want) = self.dirty_ratio_want {
            if let Some(n) = self.dirty_ratio_node.as_mut() {
                if n.read_i32().unwrap_or(-1) != want {
                    let _ = n.write_i32(want);
                    crate::klog(&format!("power: dirty_ratio -> {}", want));
                }
            }
        }
        if let Some(want) = self.dirty_bg_want {
            if let Some(n) = self.dirty_bg_node.as_mut() {
                if n.read_i32().unwrap_or(-1) != want {
                    let _ = n.write_i32(want);
                    crate::klog(&format!("power: dirty_bg -> {}", want));
                }
            }
        }
    }

    pub fn freq_table(&self) -> String {
        let mut s = String::new();
        for p in self.policies.iter() {
            if p.lvl0 > 0 {
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&format!("{}k", p.lvl0 / 1000));
            }
        }
        if s.is_empty() {
            s.push_str("off");
        }
        s
    }
}
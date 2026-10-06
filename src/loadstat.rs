use std::fs::File;
use std::io::{BufRead, BufReader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuLoad {
    Quiescent,
    Background,
    Active,
    Heavy,
    Saturated,
}

impl CpuLoad {
    pub fn label(self) -> &'static str {
        match self {
            Self::Quiescent => "q",
            Self::Background => "bg",
            Self::Active => "act",
            Self::Heavy => "hvy",
            Self::Saturated => "sat",
        }
    }

    pub fn thermal_weight(self) -> f64 {
        match self {
            Self::Quiescent => 0.6,
            Self::Background => 0.8,
            Self::Active => 1.0,
            Self::Heavy => 1.25,
            Self::Saturated => 1.6,
        }
    }
}

pub struct LoadTracker {
    prev_idle: u64,
    prev_total: u64,
    emwa_load: f64,
    emwa_freq: f64,
    current: CpuLoad,
    dwell: u32,
    gpu_node: Option<String>,
    gpu_prev_busy: u64,
    gpu_prev_total: u64,
}

impl LoadTracker {
    pub fn new() -> Self {
        Self {
            prev_idle: 0,
            prev_total: 0,
            emwa_load: 0.0,
            emwa_freq: 0.0,
            current: CpuLoad::Quiescent,
            dwell: 0,
            gpu_node: Self::probe_gpu(),
            gpu_prev_busy: 0,
            gpu_prev_total: 0,
        }
    }

    fn probe_gpu() -> Option<String> {
        let paths = [
            "/sys/class/kgsl/kgsl-3d0/gpubusy",
            "/sys/class/kgsl/kgsl-3d0/gpu_busy_percentage",
            "/sys/devices/platform/soc/3d00000.qcom,kgsl-3d0/devfreq/3d00000.qcom,kgsl-3d0/gpu_load",
        ];
        paths.iter().find(|p| std::path::Path::new(p).exists()).map(|s| s.to_string())
    }

    fn snapshot_cpu() -> (u64, u64) {
        let Ok(f) = File::open("/proc/stat") else {
            return (0, 0);
        };
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if !line.starts_with("cpu ") {
                continue;
            }
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.len() < 5 {
                return (0, 0);
            }
            let user: u64 = p[1].parse().unwrap_or(0);
            let nice: u64 = p[2].parse().unwrap_or(0);
            let sys: u64 = p[3].parse().unwrap_or(0);
            let idle: u64 = p[4].parse().unwrap_or(0);
            let iow: u64 = p.get(5).and_then(|s| s.parse().ok()).unwrap_or(0);
            let irq: u64 = p.get(6).and_then(|s| s.parse().ok()).unwrap_or(0);
            let sirq: u64 = p.get(7).and_then(|s| s.parse().ok()).unwrap_or(0);
            let steal: u64 = p.get(8).and_then(|s| s.parse().ok()).unwrap_or(0);
            return (idle + iow, user + nice + sys + irq + sirq + steal + idle + iow);
        }
        (0, 0)
    }

    fn avg_freq() -> u64 {
        let mut sum = 0u64;
        let mut n = 0u32;
        for pol in [0, 3, 6, 7] {
            let path = format!(
                "/sys/devices/system/cpu/cpufreq/policy{}/scaling_cur_freq",
                pol
            );
            if let Ok(s) = std::fs::read_to_string(&path) {
                if let Ok(f) = s.trim().parse::<u64>() {
                    sum += f;
                    n += 1;
                }
            }
        }
        if n > 0 { sum / n as u64 } else { 0 }
    }

    fn poll_gpu(&mut self) -> f64 {
        let Some(ref path) = self.gpu_node else { return 0.0 };
        let Ok(raw) = std::fs::read_to_string(path) else { return 0.0 };
        let raw = raw.trim();
        if let Some((a, b)) = raw.split_once(char::is_whitespace) {
            let busy: u64 = a.parse().unwrap_or(0);
            let tot: u64 = b.parse().unwrap_or(1);
            if tot > self.gpu_prev_total {
                let db = busy.saturating_sub(self.gpu_prev_busy);
                let dt = tot.saturating_sub(self.gpu_prev_total);
                self.gpu_prev_busy = busy;
                self.gpu_prev_total = tot;
                if dt > 0 { return db as f64 / dt as f64; }
            }
            return 0.0;
        }
        raw.parse::<u64>().map(|v| v as f64 / 100.0).unwrap_or(0.0)
    }

    pub fn sample(&mut self, display_on: bool, cpu_therm: i32) -> CpuLoad {
        let (idle, total) = Self::snapshot_cpu();
        if total == 0 || self.prev_total == 0 {
            self.prev_idle = idle;
            self.prev_total = total;
            return self.current;
        }

        let d_idle = idle.saturating_sub(self.prev_idle);
        let d_total = total.saturating_sub(self.prev_total);
        self.prev_idle = idle;
        self.prev_total = total;

        let busy_ratio = if d_total > 0 { 1.0 - d_idle as f64 / d_total as f64 } else { 0.0 };

        let freq = Self::avg_freq();
        let freq_norm = if freq > 0 { freq as f64 / 2_900_000.0 } else { 0.5 };

        let gpu = self.poll_gpu();

        let a = 0.35;
        self.emwa_load = self.emwa_load * (1.0 - a) + busy_ratio * a;
        self.emwa_freq = self.emwa_freq * (1.0 - a) + freq_norm * a;

        let composite = self.emwa_load * 0.45 + self.emwa_freq * 0.30 + gpu * 0.25;

        let raw = if !display_on {
            CpuLoad::Quiescent
        } else if composite < 0.12 {
            CpuLoad::Quiescent
        } else if composite < 0.30 {
            CpuLoad::Background
        } else if composite < 0.55 {
            CpuLoad::Active
        } else if composite < 0.82 {
            CpuLoad::Heavy
        } else {
            CpuLoad::Saturated
        };

        let new = match self.current {
            CpuLoad::Quiescent | CpuLoad::Background => {
                if raw as u8 > self.current as u8 + 1 && self.dwell < 4 {
                    self.current
                } else {
                    self.dwell = 0;
                    raw
                }
            }
            CpuLoad::Saturated => {
                if (raw as u8) < self.current as u8 - 1 && self.dwell < 4 {
                    self.current
                } else {
                    self.dwell = 0;
                    raw
                }
            }
            _ => {
                if (raw as i8 - self.current as i8).abs() > 1 && self.dwell < 3 {
                    self.current
                } else {
                    self.dwell = 0;
                    raw
                }
            }
        };

        if new == self.current {
            self.dwell += 1;
        } else {
            self.dwell = 0;
        }
        self.current = new;

        if cpu_therm > 84000 && self.current as u8 >= CpuLoad::Heavy as u8 {
            self.current = CpuLoad::Active;
        }

        self.current
    }

    pub fn get(&self) -> CpuLoad {
        self.current
    }
}

use crate::loadstat::CpuLoad;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThermalRisk {
    Nominal,
    Elevated,
    High,
    Critical,
    Thermal,
}

pub struct ThermalGuard {
    violations: Vec<u64>,
    pub protection_on: bool,
    pub last_flag: &'static str,
    overtemp: bool,
}

impl ThermalGuard {
    pub fn new() -> Self {
        Self {
            violations: Vec::new(),
            protection_on: true,
            last_flag: "",
            overtemp: false,
        }
    }

    pub fn evaluate(&mut self, cpu_t: i32, board_t: i32, batt_t: i32) -> ThermalRisk {
        let now = Self::now_epoch();
        self.violations.retain(|&t| now.saturating_sub(t) < 3600);

        if cpu_t > 89000 {
            self.violations.push(now);
            self.last_flag = "cpu_critical";
            self.overtemp = true;
            self.trip_breaker();
            return ThermalRisk::Thermal;
        }
        if cpu_t > 83000 {
            self.violations.push(now);
            self.last_flag = "cpu_critical";
            self.trip_breaker();
            return ThermalRisk::Critical;
        }
        if board_t > 76000 {
            self.violations.push(now);
            self.last_flag = "board_hot";
            self.trip_breaker();
            return ThermalRisk::Critical;
        }
        if batt_t > 61000 {
            self.violations.push(now);
            self.last_flag = "cell_hot";
            self.trip_breaker();
            return ThermalRisk::High;
        }
        if batt_t > 56000 {
            self.violations.push(now);
            self.last_flag = "cell_warm";
            self.trip_breaker();
            return ThermalRisk::Elevated;
        }
        if cpu_t > 79000 {
            return ThermalRisk::Elevated;
        }
        ThermalRisk::Nominal
    }

    fn trip_breaker(&mut self) {
        if self.violations.len() >= 12 && self.protection_on {
            self.protection_on = false;
            crate::klog(&format!(
                "guard: rate-limiter tripped ({} events/hr), adaptive control off",
                self.violations.len()
            ));
        }
    }

    pub fn maybe_rearm(&mut self) {
        if !self.protection_on && self.violations.len() < 4 {
            self.protection_on = true;
            self.overtemp = false;
            crate::klog("guard: rate-limiter cleared, adaptive control restored");
        }
    }

    pub fn is_overtemp(&self) -> bool {
        self.overtemp
    }

    pub fn freq_cap_for(
        &self,
        risk: ThermalRisk,
        load: CpuLoad,
        current_cap: i32,
    ) -> i32 {
        let cap = match risk {
            ThermalRisk::Thermal => 1_100_000,
            ThermalRisk::Critical => 1_700_000,
            ThermalRisk::High => 2_050_000,
            ThermalRisk::Elevated => match load {
                CpuLoad::Saturated => 2_150_000,
                CpuLoad::Heavy => 2_250_000,
                CpuLoad::Active => 2_350_000,
                _ => 2_550_000,
            },
            ThermalRisk::Nominal => current_cap,
        };
        cap.min(current_cap)
    }

    pub fn rate(&self) -> f64 {
        self.violations.len() as f64
    }

    pub fn snapshot(&self) -> String {
        format!(
            "prot={} ovt={} cnt={}/hr flag={}",
            self.protection_on,
            self.overtemp,
            self.violations.len(),
            self.last_flag,
        )
    }

    fn now_epoch() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

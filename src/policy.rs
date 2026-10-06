#[allow(dead_code)]
pub struct StepPolicy {
    pub name: &'static str,
    trig: Vec<i32>,
    clr: Vec<i32>,
    targets: Vec<i32>,
    level: usize,
}

impl StepPolicy {
    pub fn new(name: &'static str, trig: &[i32], clr: &[i32], targets: &[i32]) -> Self {
        Self {
            name,
            trig: trig.to_vec(),
            clr: clr.to_vec(),
            targets: targets.to_vec(),
            level: 0,
        }
    }

    pub fn level(&self) -> usize {
        self.level
    }

    pub fn first_trigger(&self) -> i32 {
        self.trig.first().copied().unwrap_or(0)
    }

    pub fn target_at(&self, level: usize) -> i32 {
        if level == 0 {
            0
        } else {
            self.targets[(level - 1).min(self.targets.len() - 1)]
        }
    }

    pub fn update(&mut self, temp: i32) -> Option<(usize, i32)> {
        let max = self.trig.len();
        for i in (0..max).rev() {
            if temp >= self.trig[i] && self.level <= i {
                self.level = i + 1;
                return Some((self.level, self.targets[i]));
            }
        }
        for i in 0..max {
            if temp <= self.clr[i] && self.level > i {
                self.level = i;
                let target = if i == 0 {
                    0
                } else {
                    self.targets[i - 1]
                };
                return Some((self.level, target));
            }
        }
        None
    }

    pub fn reset(&mut self) {
        self.level = 0;
    }
}

//! Deterministic synthetic data for demos, benchmarks, and tests.

use crate::Point;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// Smooth daily-ish wave plus noise and occasional spikes.
    Cpu,
    /// Monotonic integer counter.
    Counter,
    /// A flat line.
    Constant,
    /// Uniform random values.
    Noise,
    /// Repeating ramp.
    Saw,
}

impl Pattern {
    pub fn parse(s: &str) -> Option<Pattern> {
        match s {
            "cpu" => Some(Pattern::Cpu),
            "counter" => Some(Pattern::Counter),
            "constant" => Some(Pattern::Constant),
            "noise" => Some(Pattern::Noise),
            "saw" | "sawtooth" => Some(Pattern::Saw),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Pattern::Cpu => "cpu",
            Pattern::Counter => "counter",
            Pattern::Constant => "constant",
            Pattern::Noise => "noise",
            Pattern::Saw => "saw",
        }
    }
}

/// Small deterministic PRNG so every run produces the same database.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_f64(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

pub fn generate(pattern: Pattern, n: usize, start: i64, interval_ms: i64) -> Vec<Point> {
    let mut rng = Lcg::new(0x5EED_1234_ABCD_0001);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let x = i as f64;
        let value = match pattern {
            Pattern::Cpu => {
                let base = 45.0 + 18.0 * (x / 733.0).sin() + 4.0 * (x / 37.0).sin();
                let noise = (rng.next_f64() - 0.5) * 3.0;
                if i % 9973 == 0 {
                    99.0
                } else {
                    base + noise
                }
            }
            Pattern::Counter => (i as f64) * 2.0,
            Pattern::Constant => 42.0,
            Pattern::Noise => rng.next_f64() * 100.0,
            Pattern::Saw => (i % 1000) as f64 / 10.0,
        };
        out.push(Point {
            ts: start.wrapping_add((i as i64).wrapping_mul(interval_ms)),
            value,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_monotonic() {
        let pts = generate(Pattern::Cpu, 1000, 1_700_000_000_000, 1000);
        for w in pts.windows(2) {
            assert!(w[1].ts > w[0].ts);
        }
    }

    #[test]
    fn deterministic() {
        let a = generate(Pattern::Noise, 100, 0, 1);
        let b = generate(Pattern::Noise, 100, 0, 1);
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.value.to_bits(), y.value.to_bits());
        }
    }
}

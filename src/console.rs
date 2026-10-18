//! Copies the chip's console output on PB9 to stdout.

use crate::dongle::Dongle;
use crate::power::Psu;
use eyre::Result;
use std::io::Write as _;
use std::time::{Duration, Instant};

pub struct Console {
    dongle: Dongle,
}

impl Console {
    pub fn new(dongle: Dongle) -> Self {
        Self { dongle }
    }

    /// Power-cycles the chip and copies its output for `secs`, or until
    /// interrupted.
    pub fn run(mut self, psu: &Psu, baud: u32, secs: Option<f64>) -> Result<()> {
        self.dongle.power(true)?;
        self.dongle.set_baud(baud)?;
        // Discard stale output while the chip is off.
        psu.cycle_with(Duration::from_secs(6), || self.dongle.flush())?;
        let start = Instant::now();
        let mut out = std::io::stdout().lock();
        while secs.is_none_or(|s| start.elapsed().as_secs_f64() < s) {
            let data = self.dongle.read(1024, Duration::from_millis(60))?;
            if !data.is_empty() {
                out.write_all(&data)?;
                out.flush()?;
            }
        }
        Ok(())
    }
}

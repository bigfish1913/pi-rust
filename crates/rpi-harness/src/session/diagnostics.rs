//! Opt-in numeric-only recovery timing. Does not read or copy session payloads.
use std::{io::Write, time::Instant};

fn enabled() -> bool {
    std::env::var_os("RPI_SESSION_DIAGNOSTICS").is_some_and(|value| value != "0")
}

fn emit(phase: &'static str, elapsed_ns: u128, input: usize, output: usize) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(
        stderr,
        "RPI_SESSION_DIAGNOSTICS phase={} elapsed_ns={} input={} output={}",
        phase,
        elapsed_ns.min(u64::MAX as u128),
        input,
        output,
    );
}

pub(crate) struct RecoveryPhase {
    phase: &'static str,
    started: Option<Instant>,
}

impl RecoveryPhase {
    pub(crate) fn start(phase: &'static str) -> Self {
        Self {
            phase,
            started: enabled().then(Instant::now),
        }
    }

    /// `input` and `output` units are documented per phase. Emit only on success;
    /// absence of a phase is not evidence of a completed recovery.
    pub(crate) fn finish(self, input: usize, output: usize) {
        if let Some(started) = self.started {
            emit(self.phase, started.elapsed().as_nanos(), input, output);
        }
    }
}

/// Sum disjoint measurements without logging or reading the environment per row.
/// Disabled accumulators do not sample the clock. The caller emits only after
/// successful completion; `input` is physical mutation rows and `output` is
/// measurement count, not a count of accepted entries.
pub(crate) struct RecoveryAccumulator {
    phase: &'static str,
    elapsed_ns: Option<u128>,
    samples: usize,
}

impl RecoveryAccumulator {
    pub(crate) fn new(phase: &'static str) -> Self {
        Self {
            phase,
            elapsed_ns: enabled().then_some(0),
            samples: 0,
        }
    }

    pub(crate) fn start(&self) -> Option<Instant> {
        self.elapsed_ns.map(|_| Instant::now())
    }

    pub(crate) fn record(&mut self, started: Option<Instant>) {
        if let (Some(elapsed), Some(started)) = (&mut self.elapsed_ns, started) {
            *elapsed = elapsed.saturating_add(started.elapsed().as_nanos());
            self.samples += 1;
        }
    }

    pub(crate) fn finish(self, input: usize) {
        if let Some(elapsed) = self.elapsed_ns {
            emit(self.phase, elapsed, input, self.samples);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_accumulator_does_not_start_or_record() {
        let mut timer = RecoveryAccumulator {
            phase: "test",
            elapsed_ns: None,
            samples: 0,
        };
        assert!(timer.start().is_none());
        timer.record(None);
        timer.record(Some(Instant::now()));
        assert_eq!(timer.elapsed_ns, None);
        assert_eq!(timer.samples, 0);
    }

    #[test]
    fn accumulator_counts_only_completed_measurements() {
        let mut timer = RecoveryAccumulator {
            phase: "test",
            elapsed_ns: Some(0),
            samples: 0,
        };
        timer.record(None);
        assert_eq!(timer.samples, 0);
        let started = timer.start();
        timer.record(started);
        let before = timer.elapsed_ns.unwrap();
        timer.record(timer.start());
        assert_eq!(timer.samples, 2);
        assert!(timer.elapsed_ns.unwrap() >= before);
    }
}

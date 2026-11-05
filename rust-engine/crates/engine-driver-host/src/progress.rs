use std::time::{Duration, Instant};

/// Minimum byte advance between two `Downloading` reports.
pub const PROGRESS_MIN_BYTES: u64 = 64 * 1024;
/// Minimum time between two `Downloading` reports when fewer bytes arrived.
pub const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(50);

/// Pipeline stage of a driver install, in the order they are reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstallStep {
    Downloading,
    VerifyingChecksum,
    VerifyingSignature,
    VerifyingAbi,
    Loading,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallProgress {
    pub step: InstallStep,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// Receives install progress. Called from the installing thread; keep it cheap
/// (store into an atomic / try_send), it is invoked at most every
/// [`PROGRESS_MIN_BYTES`] or [`PROGRESS_MIN_INTERVAL`] while downloading.
pub trait ProgressSink: Send + Sync {
    fn report(&self, progress: InstallProgress);
}

impl<F: Fn(InstallProgress) + Send + Sync> ProgressSink for F {
    fn report(&self, progress: InstallProgress) {
        self(progress)
    }
}

/// Sink that drops every report.
pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn report(&self, _progress: InstallProgress) {}
}

/// Rate limiter in front of a sink: a step change always reports, byte
/// progress inside a step only after [`PROGRESS_MIN_BYTES`] more bytes or
/// [`PROGRESS_MIN_INTERVAL`] more time (and at least one new byte).
pub(crate) struct ThrottledProgress<'a> {
    sink: &'a dyn ProgressSink,
    last: Option<(InstallStep, u64, Instant)>,
}

impl<'a> ThrottledProgress<'a> {
    pub(crate) fn new(sink: &'a dyn ProgressSink) -> Self {
        Self { sink, last: None }
    }

    pub(crate) fn report(&mut self, step: InstallStep, done: u64, total: u64, now: Instant) {
        let due = match self.last {
            Some((last_step, last_done, last_at)) if last_step == step => {
                done > last_done
                    && (done - last_done >= PROGRESS_MIN_BYTES
                        || now.saturating_duration_since(last_at) >= PROGRESS_MIN_INTERVAL)
            }
            _ => true,
        };
        if due {
            self.emit(step, done, total, now);
        }
    }

    /// Reports the final position of `step` unless it was just reported.
    pub(crate) fn finish(&mut self, step: InstallStep, done: u64, total: u64, now: Instant) {
        if !matches!(self.last, Some((s, d, _)) if s == step && d == done) {
            self.emit(step, done, total, now);
        }
    }

    fn emit(&mut self, step: InstallStep, done: u64, total: u64, now: Instant) {
        self.last = Some((step, done, now));
        self.sink.report(InstallProgress {
            step,
            bytes_done: done,
            bytes_total: total,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn collect(run: impl FnOnce(&mut ThrottledProgress<'_>, Instant)) -> Vec<InstallProgress> {
        let seen = Mutex::new(Vec::new());
        let sink = |progress| seen.lock().expect("lock").push(progress);
        let mut throttle = ThrottledProgress::new(&sink);
        run(&mut throttle, Instant::now());
        seen.into_inner().expect("lock")
    }

    #[test]
    fn small_chunks_in_one_instant_report_every_64_kib() {
        let reports = collect(|throttle, start| {
            for chunk in 1..=40_u64 {
                throttle.report(
                    InstallStep::Downloading,
                    chunk * 8 * 1024,
                    320 * 1024,
                    start,
                );
            }
        });
        let done: Vec<u64> = reports.iter().map(|report| report.bytes_done).collect();
        // First report, then every >= 64 KiB: 8 KiB steps => every 8th chunk.
        assert_eq!(done, vec![8192, 73_728, 139_264, 204_800, 270_336]);
    }

    #[test]
    fn slow_trickle_reports_after_the_interval() {
        let reports = collect(|throttle, start| {
            throttle.report(InstallStep::Downloading, 1, 100, start);
            throttle.report(
                InstallStep::Downloading,
                2,
                100,
                start + Duration::from_millis(49),
            );
            throttle.report(
                InstallStep::Downloading,
                3,
                100,
                start + Duration::from_millis(50),
            );
            // No new byte: nothing to say even though time passed.
            throttle.report(
                InstallStep::Downloading,
                3,
                100,
                start + Duration::from_secs(1),
            );
        });
        let done: Vec<u64> = reports.iter().map(|report| report.bytes_done).collect();
        assert_eq!(done, vec![1, 3]);
    }

    #[test]
    fn step_changes_always_report_and_finish_is_not_duplicated() {
        let reports = collect(|throttle, start| {
            throttle.report(InstallStep::Downloading, 10, 10, start);
            throttle.finish(InstallStep::Downloading, 10, 10, start);
            throttle.report(InstallStep::VerifyingChecksum, 10, 10, start);
            throttle.report(InstallStep::VerifyingSignature, 10, 10, start);
        });
        let steps: Vec<InstallStep> = reports.iter().map(|report| report.step).collect();
        assert_eq!(
            steps,
            vec![
                InstallStep::Downloading,
                InstallStep::VerifyingChecksum,
                InstallStep::VerifyingSignature
            ]
        );
    }
}

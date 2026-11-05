//! Dev-only foreground hang diagnostics (cargo feature `perf-diagnostics`).
//!
//! GPUI's profiler records every foreground task poll, action handler, input
//! dispatch, window draw and platform presentation into a journal.
//! [`gpui::App::start_hang_monitor`] drains that journal on a dedicated OS
//! thread and reports each interval that contained a single block of at least
//! [`HANG_THRESHOLD`], or whose total foreground spend reached [`FRAME_BUDGET`].
//!
//! Using the upstream monitor thread (instead of polling a `HangDetector` from
//! the 500 ms FPS sampler) adds no foreground work at all and, unlike a
//! foreground timer, still reports while the foreground thread is stalled. The
//! scenario runner never starts the FPS sampler anyway.
//!
//! Incidents are appended to the scenario telemetry stream: one JSON line per
//! incident on stdout (`"event": "hang"`), next to the `ready` / `ui-state`
//! lines the capture runner already reads from `stdout.log`.
//! `scripts/capture-gpui.py` lifts them into `telemetry.json` as
//! `hang_incidents`. The feature is off by default and is only installed for
//! UI-scenario runs (see `main.rs`).

use gpui::profiler::hang::{
    HangMonitorConfig, HangMonitorPoll, HangMonitorPollReason, SerializedHangIncident,
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// A single piece of foreground work this long is a hang.
pub const HANG_THRESHOLD: Duration = Duration::from_millis(50);
/// An interval whose total foreground spend reaches this also counts (60 Hz).
pub const FRAME_BUDGET: Duration = Duration::from_micros(16_600);
/// How often the monitor thread drains the journal.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Contributors kept per incident; the longest ones win.
const MAX_CONTRIBUTORS: usize = 8;

fn millis(duration: Duration) -> f64 {
    duration.as_micros() as f64 / 1000.0
}

/// Converts one monitor poll into telemetry events, one per incident. Empty
/// polls produce nothing. `startup` anchors the incidents' `*_ms` timestamps.
pub fn hang_events(pid: u32, startup: Instant, poll: &HangMonitorPoll) -> Vec<Value> {
    let reason = match poll.reason {
        HangMonitorPollReason::Interval => "interval",
        HangMonitorPollReason::Flush => "flush",
    };
    poll.incidents
        .iter()
        .map(|incident| {
            let incident = SerializedHangIncident::convert(
                startup,
                incident,
                MAX_CONTRIBUTORS,
                poll.first_present_at,
            );
            json!({
                "event": "hang",
                "pid": pid,
                "poll": reason,
                "threshold_ms": millis(HANG_THRESHOLD),
                "frame_budget_ms": millis(FRAME_BUDGET),
                "incident": incident,
            })
        })
        .collect()
}

fn emit_event_line(event: &Value) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{event}");
    let _ = stdout.flush();
}

/// Starts the hang monitor, writing each incident to the scenario telemetry
/// stream (stdout).
pub fn install(cx: &mut gpui::App) {
    install_with_sink(cx, |event| emit_event_line(&event));
}

/// The callback `install_with_sink` hands to the monitor thread: converts each
/// poll and passes every resulting event to `sink`.
fn on_poll(
    pid: u32,
    startup: Instant,
    mut sink: impl FnMut(Value) + Send + 'static,
) -> impl FnMut(HangMonitorPoll) + Send + 'static {
    move |poll| {
        for event in hang_events(pid, startup, &poll) {
            sink(event);
        }
    }
}

/// [`install`] with an explicit sink. The sink runs on the monitor thread.
pub fn install_with_sink(cx: &mut gpui::App, sink: impl FnMut(Value) + Send + 'static) {
    let config = HangMonitorConfig {
        threshold: HANG_THRESHOLD,
        frame_budget: FRAME_BUDGET,
        interval: POLL_INTERVAL,
    };
    let started = cx.start_hang_monitor(config, on_poll(std::process::id(), Instant::now(), sink));
    if let Err(error) = started {
        eprintln!("TableX perf diagnostics: hang monitor not started: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::profiler::hang::HangIncident;
    use gpui::profiler::journal::{
        ForegroundEvent, FrameSnapshot, InputTiming, IntervalBoundary, PresentedFrame,
    };
    use gpui::profiler::{ActionTiming, FrameTiming, PresentTiming};
    use gpui::{TestAppContext, WindowId};

    /// A presented frame whose draw blocked the foreground for 90 ms, after a
    /// 60 ms action handler and a short input dispatch. Mirrors what the
    /// journal records for a slow grid layout.
    fn blocked_draw_snapshot(startup: Instant) -> FrameSnapshot {
        let at = |ms: u64| startup + Duration::from_millis(ms);
        let window_id = WindowId::from(7u64);
        let presentation = PresentTiming {
            window_id,
            present_start: at(262),
            present_end: at(270),
            animation_interval: None,
        };
        let frame = FrameTiming {
            window_id,
            dirty_at: Some(at(100)),
            invalidations: 2,
            draw_start: at(170),
            draw_end: at(260),
        };
        FrameSnapshot {
            interval_start: at(100),
            boundary: IntervalBoundary::Presented(PresentedFrame {
                frame,
                presentation,
            }),
            events: vec![
                ForegroundEvent::Action(ActionTiming {
                    name: "tablex::SlowAction",
                    start: at(100),
                    end: at(160),
                }),
                ForegroundEvent::Input(InputTiming {
                    kind: "mouse_down",
                    start: at(160),
                    end: at(165),
                    caused_invalidation: true,
                }),
                ForegroundEvent::Draw(frame),
                ForegroundEvent::Present(presentation),
            ],
            small_polls: Vec::new(),
            dropped_events: 0,
            journal_discontinuous: false,
        }
    }

    fn poll_with(
        startup: Instant,
        snapshots: Vec<FrameSnapshot>,
        reason: HangMonitorPollReason,
    ) -> HangMonitorPoll {
        HangMonitorPoll {
            incidents: snapshots
                .into_iter()
                .filter_map(|snapshot| HangIncident::detect(snapshot, HANG_THRESHOLD, FRAME_BUDGET))
                .collect(),
            first_present_at: Some(startup + Duration::from_millis(10)),
            reason,
        }
    }

    #[test]
    fn a_blocked_draw_becomes_a_hang_telemetry_event() {
        let startup = Instant::now();
        let poll = poll_with(
            startup,
            vec![blocked_draw_snapshot(startup)],
            HangMonitorPollReason::Interval,
        );
        let events = hang_events(4242, startup, &poll);
        assert_eq!(events.len(), 1, "one incident, one telemetry line");
        let event = &events[0];

        assert_eq!(event["event"], "hang");
        assert_eq!(event["pid"], 4242);
        assert_eq!(event["poll"], "interval");
        assert_eq!(event["threshold_ms"], 50.0);
        assert_eq!(event["frame_budget_ms"], 16.6);
        let incident = &event["incident"];
        assert_eq!(incident["trigger"], "threshold");
        assert_eq!(incident["phase"], "steady");
        assert_eq!(incident["sealed_by"], "present");
        assert_eq!(incident["start_ms"], 100.0);
        assert_eq!(incident["active_ms"], 170.0);
        assert_eq!(incident["stall_ms"], 90.0);
        assert_eq!(incident["dirty_to_present_ms"], 170.0);
        assert_eq!(incident["event_count"], 4);
        let contributors = incident["contributors"].as_array().unwrap();
        // Contributors are the events at or above the 50 ms threshold, in
        // start order: the 60 ms action, then the 90 ms draw.
        assert_eq!(contributors.len(), 2, "{contributors:#?}");
        assert_eq!(contributors[0]["kind"], "action");
        assert_eq!(contributors[0]["name"], "tablex::SlowAction");
        assert_eq!(contributors[0]["duration_ms"], 60.0);
        assert_eq!(contributors[1]["kind"], "draw");
        assert_eq!(contributors[1]["duration_ms"], 90.0);
        assert_eq!(incident["contributors_elided"], 0);

        // The capture runner reads these back from stdout.log line by line.
        let line = event.to_string();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), *event);
    }

    #[test]
    fn many_small_blocks_are_reported_as_a_budget_incident_and_idle_polls_write_nothing() {
        let startup = Instant::now();
        let at = |ms: u64| startup + Duration::from_millis(ms);
        let window_id = WindowId::from(7u64);
        let presentation = PresentTiming {
            window_id,
            present_start: at(30),
            present_end: at(31),
            animation_interval: None,
        };
        let frame = FrameTiming {
            window_id,
            dirty_at: Some(at(0)),
            invalidations: 1,
            draw_start: at(20),
            draw_end: at(30),
        };
        // 3 x 10 ms: no single block reaches 50 ms but the interval spends 30 ms.
        let busy = FrameSnapshot {
            interval_start: at(0),
            boundary: IntervalBoundary::Presented(PresentedFrame {
                frame,
                presentation,
            }),
            events: vec![
                ForegroundEvent::Action(ActionTiming {
                    name: "tablex::Small",
                    start: at(0),
                    end: at(10),
                }),
                ForegroundEvent::Action(ActionTiming {
                    name: "tablex::Small",
                    start: at(10),
                    end: at(20),
                }),
                ForegroundEvent::Draw(frame),
            ],
            small_polls: Vec::new(),
            dropped_events: 0,
            journal_discontinuous: false,
        };
        let quiet = FrameSnapshot {
            events: vec![ForegroundEvent::Draw(FrameTiming {
                draw_start: at(20),
                draw_end: at(22),
                ..frame
            })],
            ..busy.clone()
        };
        let events = hang_events(
            1,
            startup,
            &poll_with(startup, vec![busy, quiet], HangMonitorPollReason::Flush),
        );
        assert_eq!(events.len(), 1, "only the over-budget interval is reported");
        assert_eq!(events[0]["poll"], "flush");
        assert_eq!(events[0]["incident"]["trigger"], "budget");
        assert_eq!(events[0]["incident"]["stall_ms"], 10.0);
        assert!(hang_events(
            1,
            startup,
            &poll_with(startup, vec![], HangMonitorPollReason::Interval)
        )
        .is_empty());
    }

    #[test]
    fn the_monitor_callback_forwards_every_incident_to_the_sink() {
        let startup = Instant::now();
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut callback = on_poll(99, startup, move |event| {
            sender.send(event).ok();
        });
        callback(poll_with(
            startup,
            vec![
                blocked_draw_snapshot(startup),
                blocked_draw_snapshot(startup),
            ],
            HangMonitorPollReason::Interval,
        ));
        callback(poll_with(startup, vec![], HangMonitorPollReason::Interval));
        let events: Vec<Value> = receiver.try_iter().collect();
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|event| event["event"] == "hang" && event["pid"] == 99));
    }

    /// The real wiring: `install_with_sink` starts GPUI's monitor thread on a
    /// live app, and a quiet app (no hangs) shuts down with its final flush
    /// delivering nothing. The test platform never presents frames, so real
    /// incidents cannot be produced here; the shape is covered above.
    #[gpui::test]
    fn the_installed_monitor_thread_starts_and_flushes_quietly(cx: &mut TestAppContext) {
        // The monitor runs on a real OS thread; shutdown parks until it flushes.
        cx.executor().allow_parking();
        let (sender, receiver) = std::sync::mpsc::channel();
        cx.update(|cx| {
            install_with_sink(cx, move |event| {
                sender.send(event).ok();
            });
        });
        assert!(receiver.try_recv().is_err());
        cx.update(|cx| cx.shutdown());
        assert!(receiver.try_recv().is_err(), "a quiet app reports no hangs");
    }
}

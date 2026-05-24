//! Provider-API **reachability** probing — the host-agnostic half of the feature.
//!
//! The *concept* lives here (the [`ConnectivityProbe`] trait, the [`ProbeOutcome`], and the
//! background [monitor](spawn_monitor) that turns probe results into [`UiEvent::Reachability`]);
//! the *provider-specific* network call (e.g. Gemini's token-free `models.list`) lives in
//! `joi-providers`, behind this trait. The monitor never names a provider.
//!
//! It runs **outside** the [`SessionManager`](crate::manager::SessionManager) actor — a slow or
//! hanging probe must never block command handling — and shares only the `UiEvent` broadcast sender
//! so the same `ui_event` stream carries reachability to every host/the webview (Seam B).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, Notify};

use crate::session::event::{Reachability, UiEvent};

/// The result of one reachability probe: a state plus optional human-readable detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// The reachability the probe determined.
    pub state: Reachability,
    /// Optional detail (e.g. an HTTP status or transport error), surfaced in the event.
    pub detail: Option<String>,
}

impl ProbeOutcome {
    /// A bare outcome with no detail.
    #[must_use]
    pub fn new(state: Reachability) -> Self {
        Self {
            state,
            detail: None,
        }
    }

    /// An outcome carrying a detail string.
    #[must_use]
    pub fn with_detail(state: Reachability, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: Some(detail.into()),
        }
    }
}

/// A token-free reachability check against a provider's API. Implemented in `joi-providers` per
/// provider; the engine drives it generically. Implementations **must not** consume tokens (use a
/// metadata/connectivity endpoint, never `generateContent`).
#[async_trait]
pub trait ConnectivityProbe: Send + Sync {
    /// Run one probe and report the outcome. Should apply its own short timeout so the monitor's
    /// cadence stays predictable.
    async fn probe(&self) -> ProbeOutcome;
}

/// Spawn the background reachability monitor on the current runtime.
///
/// Behaviour:
/// - **startup:** emit `Checking`, probe once, emit the result.
/// - **periodic:** every `interval` (when `interval > 0`), probe and emit the result *unconditionally*
///   — a steady re-emit doubles as a heartbeat so a late `ui_event` subscriber (e.g. a reloaded
///   webview) gets the current state within one interval without needing a query command.
/// - **on demand:** when `trigger` is notified (a host's `check_reachability`, or a session connect
///   failure), emit `Checking`, probe, emit the result — so a manual retry always gives feedback.
///
/// `interval == 0` disables periodic polling (startup + on-demand probes still run).
pub fn spawn_monitor(
    events: broadcast::Sender<UiEvent>,
    probe: Arc<dyn ConnectivityProbe>,
    interval: Duration,
    trigger: Arc<Notify>,
) {
    tokio::spawn(async move {
        // `Checking` then a first probe so the dot resolves at boot rather than after one interval.
        run_probe(&events, &probe, true).await;

        loop {
            // A zero interval means "no periodic poll" — park on the trigger only. Otherwise race the
            // tick against the on-demand trigger; the trigger wins as an interactive probe.
            let interactive = if interval.is_zero() {
                trigger.notified().await;
                true
            } else {
                tokio::select! {
                    () = trigger.notified() => true,
                    () = tokio::time::sleep(interval) => false,
                }
            };
            run_probe(&events, &probe, interactive).await;
        }
    });
}

/// Emit `Checking` (only for interactive/startup probes), run the probe, and emit the outcome
/// unconditionally (a heartbeat so late subscribers get the current state). A closed broadcast
/// channel just means there are no subscribers — harmless.
async fn run_probe(
    events: &broadcast::Sender<UiEvent>,
    probe: &Arc<dyn ConnectivityProbe>,
    interactive: bool,
) {
    if interactive {
        let _ = events.send(UiEvent::Reachability {
            state: Reachability::Checking,
            detail: None,
        });
    }
    let outcome = probe.probe().await;
    tracing::debug!(state = ?outcome.state, detail = ?outcome.detail, "reachability probe");
    let _ = events.send(UiEvent::Reachability {
        state: outcome.state,
        detail: outcome.detail,
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts calls and returns a fixed outcome — no network.
    struct FakeProbe {
        calls: Arc<AtomicUsize>,
        outcome: ProbeOutcome,
    }

    #[async_trait]
    impl ConnectivityProbe for FakeProbe {
        async fn probe(&self) -> ProbeOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.outcome.clone()
        }
    }

    fn fake(calls: &Arc<AtomicUsize>, state: Reachability) -> Arc<dyn ConnectivityProbe> {
        Arc::new(FakeProbe {
            calls: Arc::clone(calls),
            outcome: ProbeOutcome::new(state),
        })
    }

    fn state_of(ev: UiEvent) -> Reachability {
        match ev {
            UiEvent::Reachability { state, .. } => state,
            other => panic!("expected Reachability, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn startup_emits_checking_then_outcome() {
        let (tx, mut rx) = broadcast::channel(8);
        let calls = Arc::new(AtomicUsize::new(0));
        // interval 0 => probe at startup, then park on the trigger (no periodic ticks).
        spawn_monitor(
            tx,
            fake(&calls, Reachability::Online),
            Duration::ZERO,
            Arc::new(Notify::new()),
        );

        assert_eq!(state_of(rx.recv().await.unwrap()), Reachability::Checking);
        assert_eq!(state_of(rx.recv().await.unwrap()), Reachability::Online);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "exactly one probe at startup"
        );
    }

    #[tokio::test]
    async fn trigger_runs_another_probe() {
        let (tx, mut rx) = broadcast::channel(8);
        let calls = Arc::new(AtomicUsize::new(0));
        let trigger = Arc::new(Notify::new());
        spawn_monitor(
            tx,
            fake(&calls, Reachability::Offline),
            Duration::ZERO,
            Arc::clone(&trigger),
        );

        // Drain the startup pair, then nudge: a second interactive probe (Checking + outcome) fires.
        assert_eq!(state_of(rx.recv().await.unwrap()), Reachability::Checking);
        assert_eq!(state_of(rx.recv().await.unwrap()), Reachability::Offline);
        trigger.notify_one();
        assert_eq!(state_of(rx.recv().await.unwrap()), Reachability::Checking);
        assert_eq!(state_of(rx.recv().await.unwrap()), Reachability::Offline);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}

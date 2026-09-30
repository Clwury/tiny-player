//! Credential-free application flow diagnostics. Only static operation names
//! and process-local counters may enter this interface, never domain payloads.

#[derive(Clone, Copy, Debug)]
pub(crate) struct TraceId {
    #[cfg(debug_assertions)]
    id: u64,
    #[cfg(debug_assertions)]
    operation: &'static str,
}

impl TraceId {
    pub(crate) fn start(operation: &'static str) -> Self {
        #[cfg(debug_assertions)]
        let trace = {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT_ID: AtomicU64 = AtomicU64::new(1);
            Self {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                operation,
            }
        };
        #[cfg(not(debug_assertions))]
        let trace = {
            let _ = operation;
            Self {}
        };
        trace.record("started");
        trace
    }

    pub(crate) fn record(self, phase: &'static str) {
        #[cfg(debug_assertions)]
        tracing::debug!(
            target: "tiny_player::flow",
            trace_id = self.id,
            operation = self.operation,
            phase,
            "application flow"
        );
        #[cfg(not(debug_assertions))]
        let _ = phase;
    }
}

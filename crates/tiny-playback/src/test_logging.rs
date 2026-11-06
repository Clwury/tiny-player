use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

pub(crate) struct LogEvent {
    pub(crate) level: Level,
    pub(crate) target: &'static str,
    pub(crate) fields: BTreeMap<String, String>,
}

struct Capture {
    events: Arc<Mutex<Vec<LogEvent>>>,
    next_span: AtomicU64,
}

impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= &Level::DEBUG
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::DEBUG)
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        self.events.lock().unwrap().push(LogEvent {
            level: *event.metadata().level(),
            target: event.metadata().target(),
            fields: fields.0,
        });
    }
}

struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

pub(crate) fn capture_events(run: impl FnOnce()) -> Vec<LogEvent> {
    let events = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::with_default(
        Capture {
            events: Arc::clone(&events),
            next_span: AtomicU64::new(1),
        },
        run,
    );
    std::mem::take(&mut *events.lock().unwrap())
}

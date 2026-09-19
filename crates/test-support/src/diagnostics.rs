//! Recording diagnostics fake: captures emitted records for assertions.
use bridge_app::diagnostics::{Diagnostics, Sink};
use std::sync::{Arc, Mutex};

struct Recording {
    records: Arc<Mutex<Vec<String>>>,
}

impl Sink for Recording {
    fn write(&self, record: &str, _: bool) -> std::io::Result<()> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(record.to_owned());
        Ok(())
    }
}

/// Create a diagnostics handle plus the shared record log it writes into.
/// Each call returns an independent pair, so separate runtime instances in one
/// process can be observed without cross-talk.
pub fn recorder() -> (Diagnostics, Arc<Mutex<Vec<String>>>) {
    let records = Arc::new(Mutex::new(Vec::new()));
    (
        Diagnostics::new(Box::new(Recording {
            records: records.clone(),
        })),
        records,
    )
}

/// Snapshot of the records emitted so far.
pub fn records(records: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    records.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

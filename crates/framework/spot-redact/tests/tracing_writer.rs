//! The redacting writer test: bytes written through the wrapper come out redacted.
#![cfg(feature = "tracing")]
#![allow(clippy::expect_used)]
// The brief exempts tests from the no-expect rule.

use std::io::Write;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

use spot_redact::RedactingMakeWriter;
use spot_redact::Redactor;

/// Handle to the captured bytes; itself a `Write` (the MakeWriter closure's return type).
#[derive(Clone)]
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("no poisoning in tests")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Captures what the subscriber would write.
#[derive(Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("no poisoning in tests")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn written_events_are_redacted_before_the_sink() {
    let mut policy = spot_redact::RedactionPolicy::default();
    policy.patterns.pii_email = true;
    let r = Redactor::from_policy(&policy).expect("the test policy compiles");
    let sink = Sink::default();
    let handle = sink.0.clone();
    let writer = RedactingMakeWriter::new(Arc::new(r), move || SharedSink(handle.clone()));
    let mut w = writer.make_writer();
    w.write_all(b"login: Authorization: SkjAjN3jM2kVwP9qLb, from ops@example.org\n")
        .expect("the sink takes the event");
    w.flush().expect("flush");
    let got = sink.0.lock().expect("no poisoning in tests").clone();
    let text = String::from_utf8(got).expect("the event is utf8");
    // The header value runs to the comma; the email is a separate pii match.
    assert_eq!(
        text,
        "login: Authorization: [redacted:header], from [redacted:email]\n"
    );
}

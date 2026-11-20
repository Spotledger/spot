//! Mail queue metrics (WP-PLT-15.T5): how many messages wait to be sent.
//!
//! The crate defines the name and the sink; a host turns the call into a Prometheus series. No
//! label: the queue of one site is not told apart from another's here (rule M3 of the metrics
//! lint keeps site and recipient out of every label).

/// Gauge: mail messages waiting to be sent or retried.
pub const MAIL_QUEUE_DEPTH: &str = "spot_mail_queue_depth";

/// Where the number goes.
pub trait MailMetricsSink: Send + Sync {
    /// Sets the number of messages waiting.
    fn queue_depth(&self, depth: u64);
}

/// Publishes the depth, summed over the queues the caller counted (a host with several sites adds
/// them up before calling, or calls with each site's count and lets the sink sum).
pub fn publish(sink: &dyn MailMetricsSink, depths: &[u64]) {
    let total = depths
        .iter()
        .fold(0_u64, |sum, depth| sum.saturating_add(*depth));
    sink.queue_depth(total);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[derive(Default)]
    struct Recording(AtomicU64);

    impl MailMetricsSink for Recording {
        fn queue_depth(&self, depth: u64) {
            self.0.store(depth, Ordering::SeqCst);
        }
    }

    #[test]
    fn the_depth_moves_and_sums() {
        let sink = Recording::default();
        publish(&sink, &[2, 3]);
        assert_eq!(sink.0.load(Ordering::SeqCst), 5);
        publish(&sink, &[]);
        assert_eq!(sink.0.load(Ordering::SeqCst), 0);
        publish(&sink, &[u64::MAX, 1]);
        assert_eq!(sink.0.load(Ordering::SeqCst), u64::MAX);
    }
}

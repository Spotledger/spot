//! The certificate step (h): a node's certificate is about to expire (`CERT_EXPIRING`, amber inside
//! 30 days, red inside 3 or past), from the enrolment service's list (WP-RTB-02.T11; ADR-062 section
//! 3). Rotation needs the current key and is the node's own act; the step only says it is time.

use crate::runner::{ConditionSink, Raised, Severity, Step, StepError, StepOutcome};

/// A node certificate is expiring.
pub const CERT_EXPIRING: &str = "CERT_EXPIRING";

/// One certificate near its end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expiring {
    /// The node (an identity).
    pub node: String,
    /// Whole days left; zero or negative when it has expired.
    pub days_left: i64,
}

/// Where expiring certificates come from.
pub trait CertificateSource: Send {
    /// At most `max` certificates, soonest first.
    ///
    /// # Errors
    /// A reason when the directory cannot be read.
    fn expiring(&mut self, max: usize) -> Result<Vec<Expiring>, String>;
}

/// Windows, days.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    /// Amber inside this many days.
    pub amber_days: i64,
    /// Red inside this many days.
    pub red_days: i64,
}

impl Default for Windows {
    fn default() -> Self {
        Self {
            amber_days: 30,
            red_days: 3,
        }
    }
}

/// The step.
pub struct CertificateStep<S: CertificateSource> {
    source: S,
    windows: Windows,
    open: std::collections::BTreeSet<String>,
}

impl<S: CertificateSource> CertificateStep<S> {
    /// A step over `source`.
    pub fn new(source: S, windows: Windows) -> Self {
        Self {
            source,
            windows,
            open: Default::default(),
        }
    }
}

impl<S: CertificateSource> Step for CertificateStep<S> {
    fn name(&self) -> &'static str {
        "certificate"
    }

    fn interval_secs(&self) -> u64 {
        6 * 3_600
    }

    fn run(
        &mut self,
        _now: i64,
        max_items: usize,
        sink: &mut dyn ConditionSink,
    ) -> Result<StepOutcome, StepError> {
        let list = self
            .source
            .expiring(max_items.saturating_add(1))
            .map_err(|reason| StepError { reason })?;
        let mut outcome = StepOutcome {
            truncated: list.len() > max_items,
            ..StepOutcome::default()
        };
        let mut now_open = std::collections::BTreeSet::new();
        for cert in list.into_iter().take(max_items) {
            outcome.examined += 1;
            if cert.days_left > self.windows.amber_days {
                continue;
            }
            let severity = if cert.days_left <= self.windows.red_days {
                Severity::Red
            } else {
                Severity::Amber
            };
            now_open.insert(cert.node.clone());
            sink.raise(&Raised {
                condition: CERT_EXPIRING,
                subject: cert.node,
                severity,
            });
            outcome.raised += 1;
        }
        // A node no longer on the list (rotated) has its condition cleared.
        if !outcome.truncated {
            for node in self.open.difference(&now_open) {
                sink.clear(CERT_EXPIRING, node);
                outcome.cleared += 1;
            }
            self.open = now_open;
        } else {
            self.open.extend(now_open);
        }
        Ok(outcome)
    }
}

impl<T: CertificateSource + ?Sized> CertificateSource for Box<T> {
    fn expiring(&mut self, max: usize) -> Result<Vec<Expiring>, String> {
        (**self).expiring(max)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    struct Certs(Vec<Expiring>);
    impl CertificateSource for Certs {
        fn expiring(&mut self, max: usize) -> Result<Vec<Expiring>, String> {
            Ok(self.0.iter().take(max).cloned().collect())
        }
    }

    #[derive(Default)]
    struct Conditions(Vec<String>);
    impl ConditionSink for Conditions {
        fn raise(&mut self, r: &Raised) {
            self.0.push(format!(
                "raise {} {} {:?}",
                r.condition, r.subject, r.severity
            ));
        }
        fn clear(&mut self, c: &'static str, s: &str) {
            self.0.push(format!("clear {c} {s}"));
        }
    }

    fn cert(node: &str, days: i64) -> Expiring {
        Expiring {
            node: node.to_owned(),
            days_left: days,
        }
    }

    #[test]
    fn amber_inside_thirty_days_red_inside_three_and_a_rotated_node_clears() {
        let mut step = CertificateStep::new(
            Certs(vec![
                cert("a", 29),
                cert("b", 3),
                cert("c", -2),
                cert("d", 90),
            ]),
            Windows::default(),
        );
        let mut sink = Conditions::default();
        step.run(1, 10, &mut sink).unwrap();
        assert_eq!(
            sink.0,
            [
                "raise CERT_EXPIRING a Amber",
                "raise CERT_EXPIRING b Red",
                "raise CERT_EXPIRING c Red"
            ]
        );
        step.source.0 = vec![cert("b", 3)];
        let mut next = Conditions::default();
        step.run(2, 10, &mut next).unwrap();
        assert!(next.0.contains(&"clear CERT_EXPIRING a".to_owned()));
        assert!(next.0.contains(&"clear CERT_EXPIRING c".to_owned()));
    }
}

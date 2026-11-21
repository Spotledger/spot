//! The operator line: `edge_offline: lease 17h left, series INV 640/1000, identity ok`.

use std::time::Duration;

use crate::lease::Standing;

/// One series' state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesState {
    /// The series name.
    pub name: String,
    /// Numbers left in the held range.
    pub remaining: u64,
    /// The block size of a lease.
    pub block: u64,
}

/// The line. Names and counts only.
pub fn line(standing: Standing, series: &[SeriesState]) -> String {
    let (lease, identity) = match standing {
        Standing::Alive(left) => (format!("lease {} left", span(left)), "identity ok"),
        Standing::Lapsed => (
            "lease lapsed".to_owned(),
            "identity expired (sign-ins and privileged actions refused)",
        ),
    };
    let mut parts = vec![lease];
    for s in series {
        parts.push(format!("series {} {}/{}", s.name, s.remaining, s.block));
    }
    parts.push(identity.to_owned());
    format!("edge_offline: {}", parts.join(", "))
}

/// The full line: lease headroom, the series, the snapshot age (when a snapshot is held) and
/// whether the carrier is connected to a link at all.
///
/// `edge_offline: lease 17h left, series INV 640/1000, snapshot 7h old, identity ok`
pub fn line_full(
    standing: Standing,
    series: &[SeriesState],
    snapshot_age: Option<Duration>,
    carrier_wired: bool,
) -> String {
    let base = line(standing, series);
    let mut extra = Vec::new();
    match snapshot_age {
        Some(age) => extra.push(format!("snapshot {} old", span(age))),
        None => extra.push("no snapshot".to_owned()),
    }
    if !carrier_wired {
        extra.push("carrier not wired".to_owned());
    }
    // The identity verdict stays last, as in the short line.
    let (head, tail) = base
        .rsplit_once(", ")
        .map_or((base.clone(), String::new()), |(h, t)| {
            (h.to_owned(), t.to_owned())
        });
    if tail.is_empty() {
        return format!("{head}, {}", extra.join(", "));
    }
    format!("{head}, {}, {tail}", extra.join(", "))
}

fn span(duration: Duration) -> String {
    let minutes = duration.as_secs() / 60;
    if minutes >= 60 {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_reads_as_documented() {
        let series = [SeriesState {
            name: "INV".into(),
            remaining: 640,
            block: 1_000,
        }];
        assert_eq!(
            line(
                Standing::Alive(Duration::from_secs(17 * 3_600 + 5)),
                &series
            ),
            "edge_offline: lease 17h left, series INV 640/1000, identity ok"
        );
        assert!(line(Standing::Lapsed, &[]).contains("lease lapsed"));
    }

    #[test]
    fn the_full_line_adds_the_snapshot_age_before_the_verdict() {
        let series = [SeriesState {
            name: "INV".into(),
            remaining: 640,
            block: 1_000,
        }];
        assert_eq!(
            line_full(
                Standing::Alive(Duration::from_secs(17 * 3_600)),
                &series,
                Some(Duration::from_secs(7 * 3_600)),
                true
            ),
            "edge_offline: lease 17h left, series INV 640/1000, snapshot 7h old, identity ok"
        );
        let bare = line_full(Standing::Lapsed, &[], None, false);
        assert!(bare.contains("no snapshot"));
        assert!(bare.contains("carrier not wired"));
        assert!(bare.ends_with("refused)"));
    }
}

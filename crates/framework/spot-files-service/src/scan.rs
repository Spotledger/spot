//! The `file.scan` seam (ADR-08): an antivirus or policy scanner that may quarantine a file.
//!
//! The service stores the bytes, then hands a fresh read of them to the [`Scanner`]. A clean
//! verdict lets the upload finish; [`ScanVerdict::Quarantine`] keeps the record but refuses
//! every download with `fileservice.quarantined`; [`ScanVerdict::Infected`] deletes the bytes
//! and the record and refuses the upload. A scanner that errors fails the upload with
//! `file.scan_failed` and stores nothing. [`NoopScanner`] is the default (everything is
//! clean); [`FakeScanner`] answers a fixed verdict for tests.

use spot_core::SiteId;
use spot_files::store::Body;
use spot_storage_contract::RecordId;
use tokio::io::AsyncReadExt;

use crate::BoxFuture;
use crate::error::FileServiceError;

/// What a scan found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanVerdict {
    /// Nothing wrong.
    Clean,
    /// The file is kept but quarantined: downloads are refused until it is cleared.
    Quarantine {
        /// Plain-language reason from the scanner.
        reason: String,
    },
    /// The file is malware: the upload is refused and nothing is kept.
    Infected {
        /// Plain-language reason from the scanner.
        reason: String,
    },
}

/// A scan seam. The production host wires this to its antivirus; [`NoopScanner`] is the
/// default when a site has none.
pub trait Scanner: Send + Sync + 'static {
    /// Scans the stored bytes of `id`. A failure is [`FileServiceError::ScanFailed`].
    fn scan<'a>(
        &'a self,
        site: &'a SiteId,
        id: RecordId,
        body: Body,
    ) -> BoxFuture<'a, Result<ScanVerdict, FileServiceError>>;
}

/// The default scanner: everything is clean.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopScanner;

impl Scanner for NoopScanner {
    fn scan<'a>(
        &'a self,
        _site: &'a SiteId,
        _id: RecordId,
        _body: Body,
    ) -> BoxFuture<'a, Result<ScanVerdict, FileServiceError>> {
        Box::pin(async { Ok(ScanVerdict::Clean) })
    }
}

/// A scanner that answers a fixed verdict and consumes the body, for tests.
#[derive(Clone, Debug)]
pub struct FakeScanner {
    /// The verdict to return.
    pub verdict: ScanVerdict,
}

impl FakeScanner {
    /// A scanner that returns `verdict`.
    pub fn new(verdict: ScanVerdict) -> Self {
        FakeScanner { verdict }
    }
}

impl Scanner for FakeScanner {
    fn scan<'a>(
        &'a self,
        _site: &'a SiteId,
        _id: RecordId,
        mut body: Body,
    ) -> BoxFuture<'a, Result<ScanVerdict, FileServiceError>> {
        Box::pin(async move {
            // Drain the body so the seam behaves like a real scanner (which reads it).
            let mut sink = Vec::new();
            body.read_to_end(&mut sink)
                .await
                .map_err(|e| FileServiceError::ScanFailed {
                    message: format!("reading the stored bytes: {e}"),
                })?;
            Ok(self.verdict.clone())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{FakeScanner, NoopScanner, ScanVerdict, Scanner};
    use spot_core::SiteId;
    use spot_files::store::body_from_str;
    use spot_storage_contract::RecordId;

    #[tokio::test]
    async fn noop_is_clean_and_fake_answers_its_verdict() {
        let site = SiteId::new("files.test").unwrap_or_else(|e| panic!("site: {e:?}"));
        let id = RecordId::new_v7();
        assert_eq!(
            NoopScanner
                .scan(&site, id, body_from_str("x"))
                .await
                .unwrap_or_else(|e| panic!("scan: {e:?}")),
            ScanVerdict::Clean
        );
        let scanner = FakeScanner::new(ScanVerdict::Quarantine {
            reason: "review".to_owned(),
        });
        assert_eq!(
            scanner
                .scan(&site, id, body_from_str("x"))
                .await
                .unwrap_or_else(|e| panic!("scan: {e:?}")),
            ScanVerdict::Quarantine {
                reason: "review".to_owned()
            }
        );
    }
}

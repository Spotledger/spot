//! Attachments of incoming mail (WP-PLT-11.T17): the [`AttachmentWriter`] over the files
//! service's in-process upload port, so mail intake stores an attachment as a file of the
//! `run_as` user, attached to the document the message created.
//!
//! The upload goes through [`FileUpload`], which is the HTTP route's service with the same
//! checks: the type policy, the size limit, the magic-number check, the quota, the scan seam and
//! the user's right to update the document. A refusal stores nothing (the service cleans up its
//! own partial work), and the writer reports it as one of the three sink outcomes:
//!
//! - a **transient** refusal (the store, the scanner or the service was unavailable) is
//!   [`CreateError::Unavailable`]: the caller may try again;
//! - a **missing right** (`permission.denied`) is [`CreateError::Denied`]: an operator fixes the
//!   user's rights;
//! - every other refusal (too large, type refused, type mismatch, malware, quota) is
//!   [`CreateError::Invalid`]: the attachment can never be accepted.
//!
//! Neither the bytes nor the file name reach a log: a refusal logs its catalogued code only.

use std::sync::Arc;

use spot_core::SiteId;
use spot_storage_contract::RecordId;

use super::sink::{AttachmentWriter, CreateError, Fut};
use crate::services::files::{FileUpload, UploadAs, UploadRefused};

/// The permission code the files service answers when the user may not update the document.
const PERMISSION_DENIED: &str = "permission.denied";

/// Stores an attachment through the files service as the `run_as` user.
pub struct FilesAttachments {
    site: SiteId,
    upload: Arc<dyn FileUpload>,
}

impl FilesAttachments {
    /// A writer over `upload` for `site`.
    #[must_use]
    pub fn new(site: SiteId, upload: Arc<dyn FileUpload>) -> Self {
        Self { site, upload }
    }
}

/// How a refusal reaches the sink (see the module docs).
#[must_use]
pub fn classify(refused: &UploadRefused) -> CreateError {
    if refused.code == PERMISSION_DENIED {
        CreateError::Denied
    } else if refused.retryable {
        CreateError::Unavailable
    } else {
        CreateError::Invalid
    }
}

impl AttachmentWriter for FilesAttachments {
    fn supported(&self) -> bool {
        true
    }

    fn store<'a>(
        &'a self,
        run_as: RecordId,
        doctype: &'a str,
        id: RecordId,
        file_name: &'a str,
        content_type: &'a str,
        bytes: &'a [u8],
    ) -> Fut<'a, Result<(), CreateError>> {
        Box::pin(async move {
            let request = UploadAs {
                site: self.site.clone(),
                user: run_as,
                name: file_name.to_owned(),
                content_type: content_type.to_owned(),
                document: Some((doctype.to_owned(), id)),
            };
            match self.upload.upload_as(request, bytes.to_vec()).await {
                Ok(_) => Ok(()),
                Err(refused) => {
                    tracing::warn!(
                        target: "spot::mail_intake",
                        site = %self.site,
                        code = refused.code,
                        retryable = refused.retryable,
                        "an incoming attachment was refused by the files service"
                    );
                    Err(classify(&refused))
                }
            }
        })
    }
}

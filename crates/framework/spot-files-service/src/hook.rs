//! The attach lifecycle seam the document engine registers.
//!
//! The engine calls [`FileAttachHook`] in the **committed** phase of a save or delete: after a
//! document with an `attach` value is saved, [`FileAttachHook::on_document_saved`] marks the
//! named files `attached` and orphans any file that was attached to the document (or field)
//! and is no longer; after a document is deleted,
//! [`FileAttachHook::on_document_deleted`] orphans every file attached to it. A replaced or
//! cleared value is just a save whose new set no longer names the old file, so one method
//! covers both.
//!
//! [`FilesService`](crate::FilesService) implements this trait; the host registers the service
//! as the committed-phase step and tests drive it directly.

use spot_core::SiteId;
use spot_storage_contract::RecordId;

use crate::BoxFuture;
use crate::error::FileServiceError;

/// The attach lifecycle the document engine drives.
pub trait FileAttachHook: Send + Sync + 'static {
    /// A document was saved with these `attach` field values (`field -> file id`). Every named
    /// file becomes `attached` to the document through that field; every other file attached
    /// to the document (or to a field no longer named) becomes `orphaned`.
    fn on_document_saved<'a>(
        &'a self,
        site: &'a SiteId,
        doctype: &'a str,
        id: RecordId,
        attachments: &'a [(String, RecordId)],
    ) -> BoxFuture<'a, Result<(), FileServiceError>>;

    /// A document was deleted: every file attached to it becomes `orphaned`.
    fn on_document_deleted<'a>(
        &'a self,
        site: &'a SiteId,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<(), FileServiceError>>;
}

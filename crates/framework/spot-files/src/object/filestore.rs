//! The [`FileStore`] implementation itself: `put` (single or multipart off the spool),
//! `get`, `head`, `delete`, `list`, and the store's private request helpers.

use std::io::Cursor;

use object_store::path::Path as ObjectPath;
use object_store::{
    Attributes, Error as StoreError, GetOptions, GetResult, MultipartUpload, ObjectStoreExt,
    PutMode, PutMultipartOptions, PutOptions as StorePutOptions, PutPayload,
};
use spot_core::SiteId;
use tokio::io::AsyncReadExt;

use super::ObjectFileStore;
use super::metadata::{
    PART_SIZE, SINGLE_PUT_LIMIT, list_prefix, meta_of, object_path, put_attributes, store_error,
};
use super::spool::{Spool, SpoolSink};
use crate::error::FileError;
use crate::key::{FileKey, FileKeyPrefix};
use crate::store::{
    Body, FileFuture, FileMeta, FileResult, FileStore, PutOptions, StoredFile, stream_hashed,
};

impl FileStore for ObjectFileStore {
    /// `put` streams the body once through a local spool file while it hashes
    /// ([`stream_hashed`]), then sends the spooled bytes: as one conditional request
    /// for a body up to `SINGLE_PUT_LIMIT` — where the backend's `Create` mode (S3:
    /// `If-None-Match: *`) makes `overwrite = false` atomic — and in `PART_SIZE`
    /// multipart parts otherwise. The `sha256` digest and the content type travel as
    /// attributes, and the spool is removed however the put ends.
    fn put<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
        mut body: Body,
        opts: PutOptions,
    ) -> FileFuture<'a, FileResult<StoredFile>> {
        Box::pin(async move {
            let spool = Spool::new();
            let mut sink = SpoolSink::new(&spool).await?;
            let (hash, size) = stream_hashed(&mut sink, &mut body, opts.max_bytes).await?;
            // `stream_hashed` already refused anything over `opts.max_bytes`; the
            // spool holds no more than the caller allowed, and its drop removes it.
            let attributes = put_attributes(&opts.content_type, &hash);
            let object = object_path(site, &key);
            if size <= SINGLE_PUT_LIMIT {
                self.put_single(&object, &spool, &key, &opts, attributes)
                    .await?;
            } else {
                self.put_multipart(&object, &spool, size, &key, &opts, attributes)
                    .await?;
            }
            tracing::debug!(site = %site, key = %key.as_str(), size, "stored file");
            Ok(StoredFile {
                key,
                size,
                sha256: hash.0,
                content_type: opts.content_type,
            })
        })
    }

    /// `get` opens the object with a plain `GET`: the streamed body is collected into
    /// memory (the contract reads whole files) while the response carries the
    /// attributes the puts stamped — the content type rides on the response and the
    /// digest is parsed back from its `sha256` metadata. The metadata is taken off the
    /// result before the body is consumed, because `.bytes()` takes the response by
    /// value; the body hands out a `Cursor` positioned at the start.
    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<(FileMeta, Body)>> {
        Box::pin(async move {
            let result = self
                .store
                .get(&object_path(site, &key))
                .await
                .map_err(|e| store_error(e, "reading the object", site))?;
            let meta = meta_of(key, &result.meta, &result.attributes);
            tracing::debug!(site = %site, key = %meta.key.as_str(), size = meta.size, "read file");
            let bytes = result
                .bytes()
                .await
                .map_err(|e| store_error(e, "reading the object's body", site))?;
            let body: Body = Box::pin(Cursor::new(bytes));
            Ok((meta, body))
        })
    }

    /// `head` is [`ObjectFileStore::head_response`]: the same `GET` [`get`] performs
    /// under `with_head(true)`, so head and get can never disagree.
    fn head<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<Option<FileMeta>>> {
        Box::pin(async move {
            let Some(result) = self.head_response(&object_path(site, &key), site).await? else {
                return Ok(None);
            };
            let meta = meta_of(key, &result.meta, &result.attributes);
            tracing::debug!(site = %site, key = %meta.key.as_str(), size = meta.size, "file metadata");
            Ok(Some(meta))
        })
    }

    /// `delete` HEADs the object first — S3's DeleteObject is idempotent, so a raw
    /// delete would report success for a key that was never there — and answers
    /// whether the file existed. A file deleted between the preflight and the
    /// removal still counts as deleted.
    fn delete<'a>(&'a self, site: &'a SiteId, key: FileKey) -> FileFuture<'a, FileResult<bool>> {
        Box::pin(async move {
            if self.head(site, key.clone()).await?.is_none() {
                return Ok(false);
            }
            self.store
                .delete(&object_path(site, &key))
                .await
                .map_err(|e| store_error(e, "deleting the object", site))?;
            tracing::debug!(site = %site, key = %key.as_str(), "deleted file");
            Ok(true)
        })
    }

    /// `list` walks the site's objects with `list_with_delimiter` — one call per
    /// directory level, walking every common prefix the delimiter hands back, which
    /// keeps this module free of a streaming dependency — and then pages in
    /// memory: entries are filtered down to the requested prefix (the backend's
    /// prefix match is textual and would pair `site/priv` with `site/privX`), are
    /// parsed back into keys, pass the `after` cursor by key text, are ordered by key
    /// text and finally cut at `limit`. Only the surviving page — at most `limit`
    /// entries — is then HEADed for the attributes the listing does not carry, so a
    /// page of 100 costs 100 HEADs, not one per object under the prefix. An entry
    /// whose HEAD reports `NotFound` (deleted between the listing and the HEAD) drops
    /// out of the page; every other HEAD error fails the listing, because silently
    /// dropping a live key would make paging by `after` lose it forever.
    fn list<'a>(
        &'a self,
        site: &'a SiteId,
        prefix: &'a FileKeyPrefix,
        after: Option<&'a FileKey>,
        limit: usize,
    ) -> FileFuture<'a, FileResult<Vec<FileMeta>>> {
        Box::pin(async move {
            let site_prefix = format!("{site}/");
            let cursor = after.map(|key| key.as_str().to_owned());
            let mut candidates: Vec<(FileKey, ObjectPath)> = Vec::new();
            // The delimiter stops the listing at "/" levels — a key inside a directory
            // never appears at its parent — so each common prefix it returns is walked
            // too, until the store's own tree under the prefix is exhausted.
            let mut levels = vec![list_prefix(site, prefix)];
            while let Some(level) = levels.pop() {
                let delimited = self
                    .store
                    .list_with_delimiter(Some(&level))
                    .await
                    .map_err(|e| store_error(e, "listing the objects", site))?;
                levels.extend(delimited.common_prefixes);
                for meta in &delimited.objects {
                    // The site prefix comes off and the rest is re-validated, because a
                    // listing yields path text until it is parsed. Anything the textual
                    // prefix overmatch pulled in leaves here.
                    let Some(text) = meta.location.as_ref().strip_prefix(&site_prefix) else {
                        continue;
                    };
                    let Ok((_, key)) = FileKey::parse(text) else {
                        continue;
                    };
                    if !key.starts_with(prefix) {
                        continue;
                    }
                    let past_cursor = cursor.as_deref().is_none_or(|cursor| key.as_str() > cursor);
                    if !past_cursor {
                        continue;
                    }
                    candidates.push((key, meta.location.clone()));
                }
            }
            candidates.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            candidates.truncate(limit);
            let mut entries: Vec<FileMeta> = Vec::with_capacity(candidates.len());
            for (key, location) in candidates {
                match self.head_response(&location, site).await? {
                    Some(result) => entries.push(meta_of(key, &result.meta, &result.attributes)),
                    // Deleted between the listing and its HEAD: out of the page.
                    None => continue,
                }
            }
            Ok(entries)
        })
    }
}

impl ObjectFileStore {
    /// One HEAD request for an object's metadata response: the `GET` [`get`] performs
    /// under `with_head(true)` — a request the backends answer with headers (no body
    /// transfers) carrying the same attributes get's response does. object_store's own
    /// `head` is not used — it discards the attributes and would strand the digest and
    /// content type. `Ok(None)` is the honest `NotFound`; every other error is
    /// returned, never swallowed, so a transient failure cannot silently drop a live
    /// key from a listing page (it would then be lost to `after` paging forever).
    pub(super) async fn head_response(
        &self,
        location: &ObjectPath,
        site: &SiteId,
    ) -> FileResult<Option<GetResult>> {
        match self
            .store
            .get_opts(location, GetOptions::new().with_head(true))
            .await
        {
            Ok(result) => Ok(Some(result)),
            Err(StoreError::NotFound { .. }) => Ok(None),
            Err(e) => Err(store_error(e, "reading the object's metadata", site)),
        }
    }

    /// Writes one spooled body with a single PUT, whose `mode` decides Create vs
    /// Overwrite.
    ///
    /// `attributes` is consumed (rather than borrowed) because `options` takes it by
    /// value; the content type and the digest are already inside it.
    async fn put_single(
        &self,
        object: &ObjectPath,
        spool: &Spool,
        key: &FileKey,
        opts: &PutOptions,
        attributes: Attributes,
    ) -> FileResult<()> {
        let options = StorePutOptions {
            mode: if opts.overwrite {
                PutMode::Overwrite
            } else {
                PutMode::Create
            },
            attributes,
            ..StorePutOptions::default()
        };
        let bytes = tokio::fs::read(&spool.path)
            .await
            .map_err(|e| FileError::io(format!("reading the spool file back: {e}")))?;
        match self
            .store
            .put_opts(object, PutPayload::from(bytes), options)
            .await
        {
            Ok(_) => Ok(()),
            Err(StoreError::AlreadyExists { .. }) => Err(FileError::AlreadyExists {
                key: key.as_str().to_owned(),
            }),
            Err(e) => Err(FileError::io(format!("storing the object: {e}"))),
        }
    }

    /// Writes one spooled body as a multipart upload of `PART_SIZE` parts.
    ///
    /// `PutMultipartOptions` has no conditional mode, so `overwrite = false` is guarded
    /// with a HEAD before initiating — two concurrent creates of the same large key can
    /// both race past the guard and succeed, which is the module's documented
    /// compromise. The parts stream off the spool file rather than memory: all non-final
    /// parts are exactly `PART_SIZE` (AWS wants at least 5 MiB, R2 wants them equal),
    /// the final one takes the remainder. Any part failure or a failed completion
    /// aborts the upload ([`ObjectFileStore::abort_multipart`]) so its parts cannot
    /// linger in the bucket or be completed into an object.
    async fn put_multipart(
        &self,
        object: &ObjectPath,
        spool: &Spool,
        size: u64,
        key: &FileKey,
        opts: &PutOptions,
        attributes: Attributes,
    ) -> FileResult<()> {
        if !opts.overwrite {
            match self.store.head(object).await {
                Ok(_) => {
                    return Err(FileError::AlreadyExists {
                        key: key.as_str().to_owned(),
                    });
                }
                Err(StoreError::NotFound { .. }) => {}
                Err(e) => {
                    return Err(FileError::io(format!(
                        "checking the object before its multipart upload: {e}"
                    )));
                }
            }
        }
        let options = PutMultipartOptions {
            attributes,
            ..PutMultipartOptions::default()
        };
        let mut upload = self
            .store
            .put_multipart_opts(object, options)
            .await
            .map_err(|e| FileError::io(format!("initiating the multipart upload: {e}")))?;
        let mut file = tokio::fs::File::open(&spool.path)
            .await
            .map_err(|e| FileError::io(format!("reading the spool file back: {e}")))?;
        let mut remaining = size;
        while remaining > 0 {
            let part_len = remaining.min(PART_SIZE as u64) as usize;
            let mut part = vec![0u8; part_len];
            if let Err(e) = file.read_exact(&mut part).await {
                let failure = FileError::io(format!("reading a multipart part back: {e}"));
                return Err(self.abort_multipart(upload, failure).await);
            }
            if let Err(e) = upload.put_part(PutPayload::from(part)).await {
                let failure = FileError::io(format!("uploading a multipart part: {e}"));
                return Err(self.abort_multipart(upload, failure).await);
            }
            remaining -= part_len as u64;
        }
        match upload.complete().await {
            Ok(_) => Ok(()),
            Err(e) => {
                // A failed completion is documented behaviour-clean (no visible object),
                // but the parts of an S3 upload do not vanish on their own — S3 cannot
                // clean up on drop, so abort the upload explicitly (object_store's
                // guidance) and report the completion error.
                let failure = FileError::io(format!("completing the multipart upload: {e}"));
                Err(self.abort_multipart(upload, failure).await)
            }
        }
    }

    /// Aborts a failed multipart upload, discarding its uploaded parts so no partial
    /// object can ever be completed from them, and returns the failure that made the
    /// abort necessary. The abort itself is best effort: services without multipart
    /// cleanup (Azure blocks expire on their own) answer fine, and a real abort failure
    /// must not mask the original one.
    async fn abort_multipart(
        &self,
        mut upload: Box<dyn MultipartUpload>,
        failure: FileError,
    ) -> FileError {
        if let Err(e) = upload.abort().await {
            tracing::warn!(error = %e, "aborting a failed multipart upload did not finish");
        }
        failure
    }
}

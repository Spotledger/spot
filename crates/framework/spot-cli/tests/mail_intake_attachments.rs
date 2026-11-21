//! Mail intake stores attachments (WP-PLT-11.T17): the writer over the files service's
//! in-process upload port, from the sink's call to the port's refusals.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use spot_cli::services::files::{FileUpload, UploadAs, UploadRefused, UploadedFile};
use spot_cli::services::mail_intake::attachments::{FilesAttachments, classify};
use spot_cli::services::mail_intake::settings::{Settings, resolve_all};
use spot_cli::services::mail_intake::sink::{
    AttachmentWriter, CreateError, DirectoryUnavailable, DocumentCreator, DocumentSink, Fut, RunAs,
    SinkParts, UserDirectory,
};
use spot_cli::services::mail_intake::status::Status;
use spot_core::{ManualClock, SiteId};
use spot_http::api::service::BoxFuture;
use spot_mail::{IncomingMessage, MailAddress};
use spot_mail_intake::{DedupeKey, IntakeSink};
use spot_storage_contract::{RecordId, Value};
use time::OffsetDateTime;

const TABLE: &str = r#"
enabled = true

[[accounts]]
name = "support"
host = "imap.example.test"
username = "support@example.test"
address = "support@example.test"
password_ref = "support_pw"
target_doctype = "ticket"
run_as = "mail_robot"
store_attachments = true
max_attachments = 3
[accounts.field_map]
from = "requester"
subject = "title"
text = "body"

[secrets.support_pw]
sealed = "spsec:v1:placeholder"
aad = "mail_intake/support"
"#;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A files port that records what it was asked and answers from a script.
#[derive(Default)]
struct FakeFiles {
    requests: Mutex<Vec<(UploadAs, usize)>>,
    refuse: Mutex<Option<UploadRefused>>,
}

impl FileUpload for FakeFiles {
    fn upload_as<'a>(
        &'a self,
        request: UploadAs,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<UploadedFile, UploadRefused>> {
        Box::pin(async move {
            lock(&self.requests).push((request.clone(), bytes.len()));
            if let Some(refused) = lock(&self.refuse).clone() {
                return Err(refused);
            }
            Ok(UploadedFile {
                id: RecordId::new_v7(),
                name: request.name,
                size: bytes.len() as u64,
                content_type: request.content_type,
                digest: "00".repeat(32),
                private: request.document.is_none(),
            })
        })
    }
}

struct Users(RunAs);

impl UserDirectory for Users {
    fn find<'a>(&'a self, _name: &'a str) -> Fut<'a, Result<Option<RunAs>, DirectoryUnavailable>> {
        Box::pin(async move { Ok(Some(self.0)) })
    }
}

#[derive(Default)]
struct Creator {
    created: Mutex<Vec<(String, RecordId)>>,
}

impl DocumentCreator for Creator {
    fn create<'a>(
        &'a self,
        _run_as: RecordId,
        doctype: &'a str,
        id: RecordId,
        _command_id: String,
        _fields: Vec<(String, Value)>,
    ) -> Fut<'a, Result<(), CreateError>> {
        Box::pin(async move {
            lock(&self.created).push((doctype.to_owned(), id));
            Ok(())
        })
    }
}

fn message(attachments: &[(&str, &str, Vec<u8>)]) -> IncomingMessage {
    IncomingMessage {
        uid: 1,
        raw_size: 200,
        truncated: false,
        message_id: Some("<a@x>".to_owned()),
        in_reply_to: None,
        references: Vec::new(),
        from: Some(MailAddress {
            name: None,
            address: "customer@elsewhere.test".to_owned(),
        }),
        to: Vec::new(),
        cc: Vec::new(),
        reply_to: Vec::new(),
        subject: Some("Secret subject line".to_owned()),
        date: Some(1_700_000_000),
        text: Some("Secret body text".to_owned()),
        html: None,
        attachments: attachments
            .iter()
            .map(|(name, kind, bytes)| {
                spot_mail::Attachment::new(*name, *kind, bytes.clone()).unwrap()
            })
            .collect(),
        auto_submitted: false,
        headers: Vec::new(),
    }
}

struct Rig {
    sink: DocumentSink,
    account: spot_mail_intake::Account,
    files: Arc<FakeFiles>,
    creator: Arc<Creator>,
    robot: RunAs,
}

fn rig() -> Rig {
    let table: Settings = toml::from_str(TABLE).unwrap();
    let (ready, invalid) = resolve_all(&table);
    assert!(invalid.is_empty(), "{invalid:?}");
    let ready = ready.into_iter().next().unwrap();
    let robot = RunAs {
        id: RecordId::new_v7(),
        active: true,
    };
    let files = Arc::new(FakeFiles::default());
    let creator = Arc::new(Creator::default());
    let writer = FilesAttachments::new(
        SiteId::new("acme").unwrap(),
        Arc::clone(&files) as Arc<dyn FileUpload>,
    );
    let sink = DocumentSink::new(SinkParts {
        site: "acme".to_owned(),
        targets: BTreeMap::from([(ready.account.name.clone(), ready.target.clone())]),
        users: Arc::new(Users(robot)),
        creator: Arc::clone(&creator) as Arc<dyn DocumentCreator>,
        attachments: Arc::new(writer),
        status: Arc::new(Status::new()),
        clock: Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    });
    Rig {
        sink,
        account: ready.account,
        files,
        creator,
        robot,
    }
}

fn key() -> DedupeKey {
    DedupeKey("ab".repeat(32))
}

#[test]
fn the_writer_reports_that_attachments_are_supported() {
    let files = Arc::new(FakeFiles::default()) as Arc<dyn FileUpload>;
    assert!(FilesAttachments::new(SiteId::new("acme").unwrap(), files).supported());
}

#[tokio::test]
async fn an_attachment_is_stored_as_the_run_as_user_and_linked_to_the_document() {
    let r = rig();
    let m = message(&[("scan.pdf", "application/pdf", vec![1, 2, 3, 4])]);
    r.sink.accept(&r.account, &m, &key()).await.unwrap();

    let (doctype, id) = lock(&r.creator.created)[0].clone();
    let requests = lock(&r.files.requests);
    assert_eq!(requests.len(), 1);
    let (request, len) = &requests[0];
    assert_eq!(request.user, r.robot.id, "uploaded as the run_as user");
    assert_eq!(
        request.document,
        Some((doctype, id)),
        "linked to the created document"
    );
    assert_eq!(request.name, "scan.pdf");
    assert_eq!(request.content_type, "application/pdf");
    assert_eq!(*len, 4);
}

#[tokio::test]
async fn each_refusal_is_classified_by_its_catalogued_code() {
    let refuse = |code, retryable| UploadRefused { code, retryable };
    assert_eq!(
        classify(&refuse("file.too_large", false)),
        CreateError::Invalid
    );
    assert_eq!(
        classify(&refuse("file.virus_detected", false)),
        CreateError::Invalid
    );
    assert_eq!(
        classify(&refuse("fileservice.type_mismatch", false)),
        CreateError::Invalid
    );
    assert_eq!(
        classify(&refuse("api.invalid_request", false)),
        CreateError::Invalid
    );
    assert_eq!(
        classify(&refuse("permission.denied", false)),
        CreateError::Denied
    );
    assert_eq!(
        classify(&refuse("file.store_failed", true)),
        CreateError::Unavailable
    );
    assert_eq!(
        classify(&refuse("file.scan_failed", true)),
        CreateError::Unavailable
    );
}

#[tokio::test]
async fn an_oversize_file_is_refused_and_the_message_is_still_one_document() {
    let r = rig();
    *lock(&r.files.refuse) = Some(UploadRefused {
        code: "file.too_large",
        retryable: false,
    });
    let m = message(&[("big.bin", "application/octet-stream", vec![0; 8])]);
    let writer = FilesAttachments::new(
        SiteId::new("acme").unwrap(),
        Arc::clone(&r.files) as Arc<dyn FileUpload>,
    );
    let refused = writer
        .store(
            r.robot.id,
            "ticket",
            RecordId::new_v7(),
            "big.bin",
            "application/octet-stream",
            &[0; 8],
        )
        .await;
    assert_eq!(refused, Err(CreateError::Invalid));
    // Through the sink the refused attachment is counted as skipped; the document stands.
    r.sink.accept(&r.account, &m, &key()).await.unwrap();
    assert_eq!(lock(&r.creator.created).len(), 1);
}

#[tokio::test]
async fn a_user_without_the_right_is_refused() {
    let r = rig();
    *lock(&r.files.refuse) = Some(UploadRefused {
        code: "permission.denied",
        retryable: false,
    });
    let writer = FilesAttachments::new(
        SiteId::new("acme").unwrap(),
        Arc::clone(&r.files) as Arc<dyn FileUpload>,
    );
    let result = writer
        .store(
            r.robot.id,
            "ticket",
            RecordId::new_v7(),
            "a.pdf",
            "application/pdf",
            &[1],
        )
        .await;
    assert_eq!(result, Err(CreateError::Denied));
}

#[tokio::test]
async fn a_transient_store_failure_is_retriable() {
    let r = rig();
    *lock(&r.files.refuse) = Some(UploadRefused {
        code: "file.store_failed",
        retryable: true,
    });
    let writer = FilesAttachments::new(
        SiteId::new("acme").unwrap(),
        Arc::clone(&r.files) as Arc<dyn FileUpload>,
    );
    let result = writer
        .store(
            r.robot.id,
            "ticket",
            RecordId::new_v7(),
            "a.pdf",
            "application/pdf",
            &[1],
        )
        .await;
    assert_eq!(result, Err(CreateError::Unavailable));
}

/// Collects every event of the current thread as text.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

struct Visitor<'a>(&'a mut String);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={value:?} ", field.name()));
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut Visitor(&mut line));
        let mut out = lock(&self.0);
        out.push_str(&line);
        out.push('\n');
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn no_attachment_name_or_bytes_reach_a_log() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let r = rig();
    *lock(&r.files.refuse) = Some(UploadRefused {
        code: "file.virus_detected",
        retryable: false,
    });
    let m = message(&[(
        "payroll-SECRET-name.pdf",
        "application/pdf",
        b"SECRET-BYTES".to_vec(),
    )]);
    r.sink.accept(&r.account, &m, &key()).await.unwrap();
    let logged = lock(&capture.0).clone();
    assert!(
        logged.contains("file.virus_detected"),
        "the code is logged: {logged}"
    );
    for forbidden in [
        "payroll-SECRET-name",
        "SECRET-BYTES",
        "customer@elsewhere",
        "Secret subject",
    ] {
        assert!(
            !logged.contains(forbidden),
            "the log leaks {forbidden}: {logged}"
        );
    }
}

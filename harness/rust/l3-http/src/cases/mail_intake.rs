//! Incoming mail (WP-PLT-11.T13, run since WP-PLT-11.T15): messages in a mailbox become
//! documents of the configured DocType created as the `run_as` user, a repeat poll creates none,
//! an auto-reply is dropped and a message the DocType refuses is rejected without stopping the
//! next one.
//!
//! The mailbox is the fake IMAP server of `spot-mail-fake-imap`. The host reaches IMAP
//! over TLS only and trusts the platform root store, so a run needs the fake server to present a
//! certificate the process already trusts. The operator of the run supplies one (the same three
//! lines in CI and on a laptop):
//!
//! ```text
//! openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=127.0.0.1 \
//!   -addext subjectAltName=IP:127.0.0.1 -addext basicConstraints=critical,CA:FALSE \
//!   -keyout key.pem -out cert.pem
//! export SSL_CERT_FILE=$PWD/cert.pem L3_IMAP_CERT=$PWD/cert.pem L3_IMAP_KEY=$PWD/key.pem
//! ```
//!
//! Without them the case fails and says so (it never passes silently). An account-level
//! `root_certificate_pem` setting would remove the need; see the task report.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spot_mail_fake_imap::{ImapFakeServer, plain_message, spawn_tls_with_identity};
use spot_secrets::{LocalKeyring, SecretBox, SecretString};

use super::CaseDef;
use crate::fixture::{Fixture, MANAGER, Prepare};
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

/// Runs after `function` (the last registered case).
pub const ORDER: u32 = 50;

const SECTION: &str = "Incoming mail";
/// The DocType the case adds to the fixture's private copy of the sample app.
const DOCTYPE: &str = "mail_ticket";
/// The IMAP password the fake server accepts; it must reach no log.
const PASSWORD: &str = "imap-l3-password-4f9a";
/// The associated data the password is sealed under (the settings carry the same text).
const AAD: &str = "mail_intake/support";

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "mail_intake: two_messages_become_two_documents_as_the_run_as_user_and_a_repeat_poll_creates_none",
        SECTION,
        "with `[services.mail_intake]` pointing at the fake IMAP server, two messages create two documents whose creator is the `run_as` user, a second poll creates none, an auto-reply is dropped, and a message the DocType refuses is rejected without stopping the next one",
        false,
        |_| Box::pin(two_messages_become_two_documents()),
    )]
}

async fn two_messages_become_two_documents() -> CaseResult {
    let identity = Identity::from_env()?;
    // The mailbox comes first: the service is built when the host starts, so its settings (the
    // fake server's port, the sealed password) are in `common.toml` before the start.
    let server = mailbox(&identity).await?;
    let sealed = Arc::new(Mutex::new(String::new()));
    let prepare_sealed = Arc::clone(&sealed);
    let port = server.port();
    let prepare: Prepare = Box::new(move |root, site| {
        Box::pin(async move {
            let table = prepare(root, site, port).await?;
            *prepare_sealed.lock().unwrap_or_else(|p| p.into_inner()) = table.1;
            Ok(table.0)
        })
    });
    let Some(mut fixture) = Fixture::start_secondary_prepared(prepare).await else {
        return Err(failure!("no PostgreSQL for the case's own fixture"));
    };
    let sealed = sealed.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let result = run(&fixture, &server, &sealed).await;
    fixture.finish().await;
    result
}

/// The certificate and key the fake server presents, read from the run's environment.
struct Identity {
    certificate: String,
    key: String,
}

impl Identity {
    fn from_env() -> Result<Self, Failure> {
        let read = |variable: &str| -> Result<String, Failure> {
            let path = std::env::var(variable).map_err(|_| {
                failure!(
                    "`{variable}` is not set: the mail intake case needs a certificate the process trusts (see the module docs of cases/mail_intake.rs)"
                )
            })?;
            std::fs::read_to_string(&path)
                .map_err(|e| failure!("cannot read the file `{variable}` names: {e}"))
        };
        let certificate = read("L3_IMAP_CERT")?;
        let key = read("L3_IMAP_KEY")?;
        let trusted = std::env::var("SSL_CERT_FILE")
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .is_some_and(|trusted| trusted.contains(certificate.trim()));
        ensure!(
            trusted,
            "`SSL_CERT_FILE` does not name a file holding the certificate of `L3_IMAP_CERT`, so the host would refuse the fake server's TLS"
        );
        Ok(Self { certificate, key })
    }
}

const DOCTYPE_TOML: &str = r#"name = "mail_ticket"
label = "Mail Ticket"
app = "sample"
module = "projects"
title_field = "title"

[[fields]]
name = "requester"
label = "Requester"
type = "text"
max_chars = 200
required = true

[[fields]]
name = "title"
label = "Title"
type = "text"
max_chars = 40
required = true

[[fields]]
name = "description"
label = "Description"
type = "long_text"
"#;

const GRANT_TOML: &str = r#"
[[entries]]
role = "Project Manager"
doctype = "mail_ticket"
rights = ["read", "write", "create", "delete", "report", "select"]
"#;

/// Before the host starts: the DocType and the right to create it (on the fixture's private copy
/// of the sample app), the table for it, and the sealed IMAP password. Returns the `common.toml`
/// tables and the sealed text.
async fn prepare(root: &std::path::Path, site: &str, port: u16) -> Result<(String, String), String> {
    let sample = root.join("sample");
    write(&sample.join(format!("doctypes/{DOCTYPE}.toml")), DOCTYPE_TOML)
        .map_err(|e| e.to_string())?;
    let grants = sample.join("permissions/grants.toml");
    let text = std::fs::read_to_string(&grants).map_err(|e| format!("cannot read the sample grants: {e}"))?;
    write(&grants, &format!("{text}\n{GRANT_TOML}")).map_err(|e| e.to_string())?;
    // The new DocType's table.
    spot_cli::ops::install_app(
        root,
        "sample",
        None,
        spot_cli::ops::Install {
            apply: true,
            seed: false,
        },
    )
    .await
    .map_err(|e| format!("install-app sample: {e}"))?;

    // The fixture's key file (`[services.secrets] kek_file`) exists already; the IMAP password is
    // sealed under it and never written in clear.
    let kek = root.join("keys").join(format!("{site}.json"));
    let keyring = LocalKeyring::open(&kek)
        .await
        .map_err(|e| format!("cannot open the key file: {e}"))?;
    let sealed = SecretBox::new(keyring)
        .seal(&SecretString::new(PASSWORD), AAD.as_bytes())
        .await
        .map_err(|e| format!("cannot seal the IMAP password: {e}"))?
        .to_string();
    // The operator turns the service on (it is read once, when the host starts).
    let table = format!(
        r#"[services.mail_intake]
enabled = true
store = "postgres"
lease_seconds = 60

[[services.mail_intake.accounts]]
name = "support"
host = "127.0.0.1"
port = {port}
security = "tls"
username = "support@spot.test"
address = "support@spot.test"
password_ref = "support_imap"
mailbox = "INBOX"
poll_interval_seconds = 30
timeout_seconds = 10
target_doctype = "{DOCTYPE}"
run_as = "manager@l3.test"

[services.mail_intake.accounts.field_map]
from = "requester"
subject = "title"
text = "description"

[services.mail_intake.secrets.support_imap]
sealed = {sealed:?}
aad = {AAD:?}
"#
    );
    Ok((table, sealed))
}

/// The mailbox: a good message, one the DocType refuses (the title is too long), an auto-reply
/// and a second good message.
async fn mailbox(identity: &Identity) -> Result<ImapFakeServer, Failure> {
    let server = spawn_tls_with_identity(&identity.certificate, &identity.key)
        .await
        .map_err(|e| failure!("cannot start the fake IMAP server: {e}"))?;
    let long_subject = "a subject far longer than the forty characters the DocType allows";
    for (id, from, subject, body, extra) in [
        (
            "l3-1@spot.test",
            "rana@customer.test",
            "Order is late",
            "first",
            vec![],
        ),
        (
            "l3-2@spot.test",
            "omar@customer.test",
            long_subject,
            "refused",
            vec![],
        ),
        (
            "l3-3@spot.test",
            "noreply@customer.test",
            "Out of office",
            "auto",
            vec![("Auto-Submitted", "auto-replied")],
        ),
        (
            "l3-4@spot.test",
            "sara@customer.test",
            "Invoice question",
            "second",
            vec![],
        ),
    ] {
        server
            .push_message(plain_message(
                id,
                from,
                "support@spot.test",
                subject,
                body,
                &extra,
            ))
            .await;
    }
    Ok(server)
}

async fn run(fixture: &Fixture, server: &ImapFakeServer, sealed: &str) -> CaseResult {
    // 5. Two documents arrive; the refused message and the auto-reply create none.
    let rows = match until_documents(fixture, 2).await {
        Ok(rows) => rows,
        Err(failure) => {
            let ready = fixture
                .client()
                .health("ready")
                .await
                .map(|reply| reply.redacted_text())
                .unwrap_or_default();
            let polls = server.recordings().await.len();
            return Err(failure!(
                "{failure}; the fake server saw {polls} session(s); readiness: {ready}"
            ));
        }
    };
    let mut titles: Vec<String> = rows
        .iter()
        .filter_map(|row| row["title"].as_str().map(str::to_owned))
        .collect();
    titles.sort();
    ensure!(
        titles == ["Invoice question", "Order is late"],
        "the documents are not the two good messages: {titles:?}"
    );

    // 6. The documents exist for the `run_as` user, whose create right the DocType's grant gives
    //    (only the Project Manager role may create a Mail Ticket). The API does not tell who
    //    created a document (the engine stamps no `created_by` and the DocType declares none),
    //    so the creator itself is covered by the unit tests of the intake sink, not here.
    let manager = fixture.sign_in(MANAGER).await?;
    for row in &rows {
        let id = row["id"]
            .as_str()
            .ok_or_else(|| failure!("a row has no id: {row}"))?;
        manager.document(DOCTYPE, id).await?.expect(200)?;
    }

    // 7. A second poll finds the same mailbox and creates nothing.
    until_polls(server, 2).await?;
    let again = documents(fixture).await?;
    ensure!(
        again.len() == 2,
        "a repeat poll changed the document count to {}",
        again.len()
    );

    // 8. The mailbox was only read, and the password reached no log.
    for recording in server.recordings().await {
        for command in &recording.commands {
            ensure!(
                !["STORE", "EXPUNGE", "COPY", "MOVE", "DELETE", "APPEND"]
                    .iter()
                    .any(|verb| command.starts_with(verb)),
                "intake sent a state-changing command: {command}"
            );
        }
    }
    let log = std::fs::read_to_string(fixture.log()).unwrap_or_default();
    ensure!(
        !log.contains(PASSWORD),
        "the IMAP password reached the server log"
    );
    ensure!(
        !log.contains(sealed),
        "the sealed password reached the server log"
    );
    Ok(())
}

fn write(path: &std::path::Path, text: &str) -> Result<(), Failure> {
    std::fs::write(path, text).map_err(|e| failure!("cannot write {}: {e}", path.display()))
}

/// The documents of the DocType, or none while the DocType is not served yet.
async fn documents(fixture: &Fixture) -> Result<Vec<serde_json::Value>, Failure> {
    let reply = fixture.admin().list(DOCTYPE, "limit=100").await?;
    if reply.status != 200 {
        return Ok(Vec::new());
    }
    reply.rows()
}

/// Polls until the DocType holds `count` documents (60 s budget).
async fn until_documents(
    fixture: &Fixture,
    count: usize,
) -> Result<Vec<serde_json::Value>, Failure> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let rows = documents(fixture).await?;
        if rows.len() >= count {
            return Ok(rows);
        }
        ensure!(
            Instant::now() < deadline,
            "only {} of {count} documents arrived within 60 s",
            rows.len()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Polls until the fake server has seen `sessions` connections (the poll interval is 30 s, so a
/// repeat poll needs up to about 40 s).
async fn until_polls(server: &ImapFakeServer, sessions: usize) -> Result<(), Failure> {
    let deadline = Instant::now() + Duration::from_secs(75);
    while server.recordings().await.len() < sessions {
        ensure!(
            Instant::now() < deadline,
            "the account did not poll a second time within 75 s"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Let the second poll finish its work before the count is read.
    tokio::time::sleep(Duration::from_secs(3)).await;
    Ok(())
}

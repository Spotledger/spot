//! The host of the graph cases: the shared fixture's site and users, plus an app of its own.
//!
//! The graph cases need DocTypes linked in a chain (order, receipt, invoice, payment) and a tree
//! of items, a graph service tuned for a fast feed, and a host they can stop and start again.
//! The shared [`Fixture`] gives none of those, so each case builds this: it starts a
//! [`Fixture`] (database, site, sample app, users, a first host), stops that host, adds the app
//! `chain` to the catalogue and the site, writes the `[services.graph]` settings, and starts its
//! own host through `spot_cli::start_with`, the assembly `spot serve` runs. A restart rewrites
//! the settings and starts again on a new port. `crates/framework/spot-cli/tests/e2e.rs` does the
//! same with `Options` and `Running`; `fixture.rs` is not edited.

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use spot_auth::Password;
use spot_cli::ops::Install;
use spot_cli::{Options, Running, SiteKind};
use spot_login::LoginConfig;
use tokio::sync::oneshot;

use crate::client::Client;
use crate::fixture::{ADMIN_PASSWORD, Fixture};
use crate::report::Failure;
use crate::{ensure, failure};

/// A pooled connection to `$site`'s database (a macro: the pooled type belongs to a crate this
/// one does not depend on, so no function can name it).
macro_rules! connect {
    ($host:expr, $site:expr) => {{
        let loaded = spot_cli::config::load($host.fixture.root(), false)
            .map_err(|e| failure!("cannot load the configuration: {e}"))?;
        let found = loaded
            .sites
            .iter()
            .find(|s| s.site == $site)
            .cloned()
            .ok_or_else(|| failure!("no site `{}` in the configuration", $site))?;
        let backend = spot_cli::schema::backend(&loaded, &found)
            .await
            .map_err(|e| failure!("cannot connect to the database of `{}`: {e}", $site))?;
        backend
            .pool()
            .get()
            .await
            .map_err(|e| failure!("no pooled connection: {e}"))?
    }};
}

/// The DocTypes of the fixture app, in the order a chain is built.
pub const ORDER_T: &str = "chain_order";
pub const RECEIPT_T: &str = "chain_receipt";
pub const INVOICE_T: &str = "chain_invoice";
pub const PAYMENT_T: &str = "chain_payment";
pub const ITEM_T: &str = "chain_item";

/// The edge types of the chain, as the wire names them (`.` of the canonical name is `_`).
pub const RECEIPT_EDGE: &str = "chain_receipt_source";
pub const INVOICE_EDGE: &str = "chain_invoice_receipt";
pub const PAYMENT_EDGE: &str = "chain_payment_invoice";

/// A faster feed and drift check than the defaults, so a case need not wait minutes.
pub const FAST: &str = "feed_interval_ms = 50\ndrift_interval_secs = 1\nindex_memory_mb = 64";
/// The index off: every query runs over the database.
pub const STORAGE_ONLY: &str = "index_memory_mb = 0";

/// A record id no document has (a canonical UUIDv7 shape).
pub const UNKNOWN_ID: &str = "0192f3c4-0000-7000-8000-000000000001";

/// The chain configuration the lineage cases serve (`<app>/lineage.toml`): the same chain for
/// each DocType of it, and the item tree's bill of materials.
pub const LINEAGE: &str = "\
[[chain]]
doctype = \"chain_order\"
edges = [\"chain_receipt.source\", \"chain_invoice.receipt\", \"chain_payment.invoice\"]

[[chain]]
doctype = \"chain_receipt\"
edges = [\"chain_receipt.source\", \"chain_invoice.receipt\", \"chain_payment.invoice\"]

[[chain]]
doctype = \"chain_invoice\"
edges = [\"chain_receipt.source\", \"chain_invoice.receipt\", \"chain_payment.invoice\"]

[[chain]]
doctype = \"chain_payment\"
edges = [\"chain_receipt.source\", \"chain_invoice.receipt\", \"chain_payment.invoice\"]

[[bom]]
doctype = \"chain_item\"
edge = \"chain_item.parent_item\"
qty_field = \"qty\"
";

/// What a case asks of its host.
#[derive(Default)]
pub struct Start<'a> {
    /// The lines under `[services.graph]` (empty: the defaults).
    pub graph: &'a str,
    /// The app's `lineage.toml` (none: the app ships no chain configuration).
    pub lineage: Option<&'a str>,
    /// Rule files of the app: `(file stem, text)`, written to `chain/rules/<stem>.rules`.
    pub rules: &'a [(&'a str, &'a str)],
    /// Whether a second site of the same server is made.
    pub second_site: bool,
}

/// A database, a site (and perhaps a second), the fixture app and a running host that can be
/// restarted.
pub struct GraphHost {
    /// The shared fixture: root, database, users. Its own host is stopped.
    pub fixture: Fixture,
    /// The first site's Administrator.
    pub admin: Client,
    /// The second site's name and Administrator, when asked for.
    pub second: Option<(String, Client)>,
    base_common: String,
    addr: SocketAddr,
    server: Option<Running>,
    stop: Option<oneshot::Sender<()>>,
}

impl GraphHost {
    /// Builds everything and serves it.
    pub async fn start(start: Start<'_>) -> Result<GraphHost, Failure> {
        let fixture = Fixture::start_secondary()
            .await
            .ok_or_else(|| failure!("the graph case could not start its own fixture"))?;
        // The fixture's host knows neither the app nor the settings: stop it.
        fixture.begin_shutdown();
        fixture.join_host().await;
        let root = fixture.root().to_path_buf();
        write_app(&root, &start)?;
        let common = root.join("common.toml");
        let text = std::fs::read_to_string(&common)
            .map_err(|e| failure!("cannot read {}: {e}", common.display()))?;
        let base_common = text.replace("\"sample\"]", "\"sample\", \"chain\"]");
        ensure!(
            base_common != text,
            "common.toml does not list the sample app last, the chain app cannot be added: {text}"
        );
        write_common(&root, &base_common, start.graph)?;

        // The first site gets the app now; a second site made after the catalogue changed has it
        // from `new-site` on (installing twice is a no-op).
        let site = fixture.site().to_owned();
        install(&root, &site).await?;
        let mut second_name = None;
        if start.second_site {
            let name = format!("{site}-b");
            // The secrets service needs a key file per site before the site starts.
            spot_cli::services::secrets::init_key_file(
                &root.join(format!("keys/{name}.json")),
                &name,
            )
            .await
            .map_err(|e| failure!("the key file of `{name}`: {e}"))?;
            let secret = spot_cli::new_site_of(&root, &name, "en", SiteKind::HostWide)
                .await
                .map_err(|e| failure!("new-site `{name}`: {e}"))?;
            drop(secret);
            spot_cli::users::set_password(
                &root,
                &name,
                "administrator",
                &Password::new(ADMIN_PASSWORD),
            )
            .await
            .map_err(|e| failure!("set-password on `{name}`: {e}"))?;
            install(&root, &name).await?;
            second_name = Some(name);
        }

        let (running, stop, addr) = boot(&root).await?;
        let mut host = GraphHost {
            admin: Client::new(addr, site.clone()),
            second: None,
            fixture,
            base_common,
            addr,
            server: Some(running),
            stop: Some(stop),
        };
        host.wait_ready(&site).await?;
        host.admin = host.sign_admin(&site).await?;
        if let Some(name) = second_name {
            host.wait_ready(&name).await?;
            let client = host.sign_admin(&name).await?;
            host.second = Some((name, client));
        }
        Ok(host)
    }

    /// The first site's name.
    pub fn site(&self) -> &str {
        self.fixture.site()
    }

    /// An anonymous client for `site`.
    pub fn anonymous(&self, site: &str) -> Client {
        Client::new(self.addr, site)
    }

    async fn sign_admin(&self, site: &str) -> Result<Client, Failure> {
        self.anonymous(site)
            .sign_in("administrator", ADMIN_PASSWORD)
            .await
    }

    /// Signs a fixture user (by role name) in on the first site.
    pub async fn sign_in(&self, role: &str) -> Result<Client, Failure> {
        let user = self.fixture.user(role);
        let password = user
            .password
            .as_deref()
            .ok_or_else(|| failure!("{} has no password", user.email))?;
        self.anonymous(self.site())
            .sign_in(&user.email, password)
            .await
    }

    async fn wait_ready(&self, site: &str) -> Result<(), Failure> {
        let client = self.anonymous(site);
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last: String;
        loop {
            match client.health("ready").await {
                Ok(reply) if reply.status == 200 => return Ok(()),
                Ok(reply) => last = format!("HTTP {}: {}", reply.status, reply.text()),
                Err(e) => last = format!("{e}"),
            }
            ensure!(
                Instant::now() < deadline,
                "the site `{site}` was not ready within 60 s (last answer: {last})"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Stops the host and waits for its server loop (15 s at most).
    pub async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(running) = self.server.take() {
            let _ = tokio::time::timeout(Duration::from_secs(15), running.join()).await;
        }
    }

    /// Stops the host, rewrites the `[services.graph]` lines and starts it again on a new port.
    /// The Administrators sign in again; any other client of the case is stale.
    pub async fn restart(&mut self, graph: &str) -> Result<(), Failure> {
        self.stop().await;
        let root = self.fixture.root().to_path_buf();
        write_common(&root, &self.base_common, graph)?;
        let (running, stop, addr) = boot(&root).await?;
        self.server = Some(running);
        self.stop = Some(stop);
        self.addr = addr;
        let site = self.site().to_owned();
        self.wait_ready(&site).await?;
        self.admin = self.sign_admin(&site).await?;
        if let Some((name, _)) = self.second.take() {
            self.wait_ready(&name).await?;
            let client = self.sign_admin(&name).await?;
            self.second = Some((name, client));
        }
        Ok(())
    }

    /// Creates a document as `client` and returns `(id, version)`.
    pub async fn make(
        &self,
        client: &Client,
        doctype: &str,
        body: &Json,
    ) -> Result<(String, i64), Failure> {
        let reply = client.create(doctype, body).await?;
        reply.expect(201)?;
        Ok((reply.id()?, reply.version()?))
    }

    /// Runs one statement on a site's database (the edge table is changed behind the feed's back
    /// by the cases that need it).
    pub async fn execute(&self, site: &str, sql: &str) -> Result<u64, Failure> {
        let client = connect!(self, site);
        client
            .execute(sql, &[])
            .await
            .map_err(|e| failure!("`{sql}`: {e}"))
    }

    /// The first column of the one row of `sql`, a `count(*)`.
    pub async fn count(&self, site: &str, sql: &str) -> Result<i64, Failure> {
        let client = connect!(self, site);
        let row = client
            .query_one(sql, &[])
            .await
            .map_err(|e| failure!("`{sql}`: {e}"))?;
        Ok(row.get::<_, i64>(0))
    }

    /// Stops the host, then drops the database and the root (unless `L3_KEEP=1`).
    pub async fn finish(&mut self) {
        self.stop().await;
        self.fixture.finish().await;
    }
}

impl Drop for GraphHost {
    fn drop(&mut self) {
        // A case that fails early still ends its server; the fixture's own drop removes the
        // database and the root.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

// ------------------------------------------------------------------------------ the app

fn options() -> Options {
    Options {
        clock: None,
        login: LoginConfig {
            // Every request re-reads the credential, so a changed password applies at once.
            cache_ttl: Duration::ZERO,
            ..LoginConfig::for_tests()
        },
    }
}

async fn boot(root: &Path) -> Result<(Running, oneshot::Sender<()>, SocketAddr), Failure> {
    let (stop, stopped) = oneshot::channel::<()>();
    let running = spot_cli::start_with(root, false, options(), async move {
        let _ = stopped.await;
    })
    .await
    .map_err(|e| failure!("the host did not start: {e}"))?;
    let addr = *running
        .local_addrs()
        .first()
        .ok_or_else(|| failure!("the host bound no address"))?;
    Ok((running, stop, addr))
}

async fn install(root: &Path, site: &str) -> Result<(), Failure> {
    spot_cli::ops::install_app(
        root,
        "chain",
        Some(site),
        Install {
            apply: true,
            seed: false,
        },
    )
    .await
    .map(|_| ())
    .map_err(|e| failure!("install-app chain on `{site}`: {e}"))
}

fn write_common(root: &Path, base: &str, graph: &str) -> Result<(), Failure> {
    let path = root.join("common.toml");
    std::fs::write(&path, format!("{base}\n[services.graph]\n{graph}\n"))
        .map_err(|e| failure!("cannot write {}: {e}", path.display()))
}

fn text_field(name: &str, unique: bool) -> String {
    let unique = if unique { "unique = true\n" } else { "" };
    format!(
        "\n[[fields]]\nname = \"{name}\"\nlabel = \"{name}\"\ntype = \"text\"\nmax_chars = 64\n{unique}"
    )
}

fn link_field(name: &str, target: &str, required: bool) -> String {
    format!(
        "\n[[fields]]\nname = \"{name}\"\nlabel = \"{name}\"\ntype = \"link\"\noptions = \"{target}\"\nrequired = {required}\nindex = true\n"
    )
}

fn decimal_field(name: &str) -> String {
    format!(
        "\n[[fields]]\nname = \"{name}\"\nlabel = \"{name}\"\ntype = \"decimal\"\nscale = 4\nprecision = 24\n"
    )
}

fn doctype(name: &str, fields: &str) -> String {
    format!("name = \"{name}\"\nlabel = \"{name}\"\napp = \"chain\"\nmodule = \"chain\"\n{fields}")
}

/// Writes the fixture app `chain` into the root: an order, its receipts, their invoices and the
/// payments of those (each links the one before), and an item tree whose children link their
/// parent and carry a per-unit quantity. A `Viewer` reads everything but the receipts, so the
/// middle of the chain can be hidden.
fn write_app(root: &Path, start: &Start<'_>) -> Result<(), Failure> {
    let app = root.join("chain");
    let write = |path: &str, text: &str| -> Result<(), Failure> {
        let target = app.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| failure!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, text)
            .map_err(|e| failure!("cannot write {}: {e}", target.display()))
    };
    write(
        "app.toml",
        "[app]\nname = \"chain\"\nversion = \"1.0.0\"\ntitle = \"Chain: documents that link one another\"\n",
    )?;
    write(
        "doctypes/chain_order.toml",
        &doctype(
            ORDER_T,
            &(text_field("ref", false) + &text_field("note", false)),
        ),
    )?;
    write(
        "doctypes/chain_receipt.toml",
        &doctype(
            RECEIPT_T,
            &(link_field("source", ORDER_T, true) + &text_field("note", false)),
        ),
    )?;
    write(
        "doctypes/chain_invoice.toml",
        &doctype(
            INVOICE_T,
            &(link_field("receipt", RECEIPT_T, true)
                + &text_field("number", true)
                + &decimal_field("amount")),
        ),
    )?;
    write(
        "doctypes/chain_payment.toml",
        &doctype(
            PAYMENT_T,
            &(link_field("invoice", INVOICE_T, true) + &decimal_field("amount")),
        ),
    )?;
    write(
        "doctypes/chain_item.toml",
        &doctype(
            ITEM_T,
            &(text_field("title", false)
                + &link_field("parent_item", ITEM_T, false)
                + &decimal_field("qty")),
        ),
    )?;
    let mut grants = String::new();
    for doctype in [ORDER_T, INVOICE_T, PAYMENT_T, ITEM_T] {
        grants.push_str(&format!(
            "[[entries]]\nrole = \"Viewer\"\ndoctype = \"{doctype}\"\nrights = [\"read\", \"select\"]\n\n"
        ));
    }
    write("permissions/grants.toml", &grants)?;
    if let Some(text) = start.lineage {
        write("lineage.toml", text)?;
    }
    for (stem, text) in start.rules {
        write(&format!("rules/{stem}.rules"), text)?;
    }
    Ok(())
}

/// Writes (or replaces) a rule file of the fixture app in a running host's root.
pub fn write_rules(root: &Path, stem: &str, text: &str) -> Result<(), Failure> {
    let dir = root.join("chain/rules");
    std::fs::create_dir_all(&dir).map_err(|e| failure!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{stem}.rules"));
    std::fs::write(&path, text).map_err(|e| failure!("cannot write {}: {e}", path.display()))
}

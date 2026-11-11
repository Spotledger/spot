//! Shared helpers for the real-PostgreSQL command tests.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]
use spot_postgres::testing::TestDb;
use spot_storage_contract::RecordId;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

pub struct Env {
    pub root: PathBuf,
    pub site: String,
    pub url: String,
    _db: TestDb,
}
impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

pub fn run(root: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_spot"))
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut input = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            let _ = input.write_all(text.as_bytes());
        }
    }
    child.wait_with_output().unwrap()
}

pub fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

impl Env {
    /// A root with the full platform app and a private copy of the sample app in its catalogue.
    /// `None` (skipped) when no PostgreSQL is configured.
    pub async fn new() -> Option<Env> {
        let db = TestDb::create().await?;
        let url = std::env::var("SPOT_TEST_PG_URL").unwrap();
        let root = std::env::temp_dir().join(format!("spot_cli_cmd_{}", RecordId::new_v7()));
        std::fs::create_dir_all(&root).unwrap();
        copy_dir(&repo().join("apps/sample"), &root.join("sample"));
        std::fs::write(
            root.join("common.toml"),
            format!(
                "apps = [{:?}, 'sample']\n[server]\nbind = ['127.0.0.1:0']\nshutdown_grace = 2\n[postgres]\nadmin_url = {url:?}\n[trust]\nrequire_signed = false\n",
                repo().join("apps/platform").display().to_string()
            ),
        )
        .unwrap();
        let site = format!(
            "spot-test-{}",
            RecordId::new_v7().to_string().replace('-', "")
        );
        Some(Env {
            root,
            site,
            url,
            _db: db,
        })
    }

    pub fn spot(&self, args: &[&str]) -> Output {
        run(&self.root, args, None)
    }

    pub fn ok(&self, args: &[&str]) -> String {
        let output = self.spot(args);
        let out = text(&output);
        assert!(output.status.success(), "{args:?} failed: {out}");
        out
    }

    /// Creates the bare site and returns the Administrator key text.
    pub fn bare_site(&self) -> String {
        let out = self.ok(&["new-site", &self.site, "--bare"]);
        out.trim().rsplit(' ').next().unwrap().to_owned()
    }

    pub fn site_file(&self) -> String {
        std::fs::read_to_string(self.root.join("sites").join(&self.site).join("site.toml")).unwrap()
    }

    pub fn sample(&self, file: &str) -> PathBuf {
        self.root.join("sample/doctypes").join(file)
    }

    /// The live schema of the site's database.
    pub async fn live(&self) -> spot_pg_migrate::LiveSchema {
        let loaded = spot_cli::config::load(&self.root, false).unwrap();
        let site = loaded.sites.iter().find(|s| s.site == self.site).unwrap();
        let backend = spot_cli::schema::backend(&loaded, site).await.unwrap();
        spot_pg_migrate::LiveSchema::read(&backend).await.unwrap()
    }
}

/// `text` with the `[[fields]]` block named `field` removed.
pub fn without_field(text: &str, field: &str) -> String {
    let mut parts = text.split("[[fields]]");
    let mut out = parts.next().unwrap().to_owned();
    let needle = format!("name = \"{field}\"");
    for part in parts {
        if !part.contains(&needle) {
            out.push_str("[[fields]]");
            out.push_str(part);
        }
    }
    out
}

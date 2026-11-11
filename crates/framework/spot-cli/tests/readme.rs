//! The commands in `apps/sample/README.md` marked `<!-- test -->` run against a real PostgreSQL,
//! so the README cannot rot.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
use spot_postgres::testing::TestDb;
use spot_storage_contract::RecordId;
use std::path::Path;

/// The bodies of the fenced `bash` blocks that follow a `<!-- test -->` line.
fn marked_blocks(readme: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut lines = readme.lines();
    while let Some(line) = lines.next() {
        if line.trim() != "<!-- test -->" {
            continue;
        }
        let Some(fence) = lines.next() else { break };
        assert_eq!(
            fence.trim(),
            "```bash",
            "a marker must precede a bash fence"
        );
        let body: Vec<&str> = lines.by_ref().take_while(|l| l.trim() != "```").collect();
        blocks.push(body.join("\n"));
    }
    blocks
}

#[test]
fn the_marker_extraction_finds_only_marked_bash_blocks() {
    let text = "```bash\nskipped\n```\n<!-- test -->\n```bash\none\ntwo\n```\n<!-- test -->\n```bash\nthree\n```\n";
    assert_eq!(
        marked_blocks(text),
        vec!["one\ntwo".to_owned(), "three".to_owned()]
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sample_readme_commands_run() {
    let Some(_db) = TestDb::create().await else {
        return;
    };
    let url = std::env::var("SPOT_TEST_PG_URL").unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let readme = std::fs::read_to_string(repo.join("apps/sample/README.md")).unwrap();
    let blocks = marked_blocks(&readme);
    assert!(blocks.len() >= 5, "the README lost its runnable blocks");

    let work = std::env::temp_dir().join(format!("spot_cli_readme_{}", RecordId::new_v7()));
    let bin = work.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(repo.join("apps").canonicalize().unwrap(), work.join("apps"))
        .unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_spot"), bin.join("spot")).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let site = format!(
        "spot-test-{}",
        RecordId::new_v7().to_string().replace('-', "")
    );
    // The README signs in with a one-time secret and chooses a password itself, so the harness
    // supplies only what the README cannot know: this test's database URL, port and site name.
    // `spot serve` inherits the script's pipes, so a block that fails must not leave it running.
    let guard = "trap 'kill $(jobs -p) 2>/dev/null || true' EXIT\n";
    let script = blocks
        .join("\n")
        .replace("spot serve &", &format!("{guard}spot serve &"))
        .replace("postgres://spot@127.0.0.1:5432/postgres", &url)
        .replace("127.0.0.1:8000", &format!("127.0.0.1:{port}"))
        // The README installs the sample from its directory, which is a development install: the
        // harness's site says so (a real site keeps the signed-only default).
        .replace(
            "chmod 600 runtime/common.toml",
            "printf '[trust]\\nrequire_signed = false\\n' >> runtime/common.toml\nchmod 600 runtime/common.toml",
        )
        .replace("demo", &site);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = tokio::task::spawn_blocking({
        let work = work.clone();
        move || {
            std::process::Command::new("bash")
                .args(["-e", "-o", "pipefail", "-c", &script])
                .current_dir(&work)
                .env("PATH", path)
                .env_remove("SPOT_ROOT")
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let _ = std::fs::remove_dir_all(&work);
    assert!(
        output.status.success(),
        "README script failed ({:?}):\n{stdout}\n{stderr}",
        output.status
    );
    assert!(stdout.contains("signed in"), "{stdout}");
    assert!(stdout.contains("\"title\":\"First project\""), "{stdout}");
    assert!(stdout.contains("already installed"), "{stdout}");
    assert!(
        !stdout.contains("violet-lantern"),
        "a password must never be printed"
    );
}

//! The operator commands against a real PostgreSQL (skipped when none is configured).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use support::{Env, run, text, without_field};

const NEW_FIELD: &str = "\n[[fields]]\nname = \"review_note\"\nlabel = \"Review Note\"\ntype = \"text\"\nmax_chars = 80\n";

fn plan_hash(out: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix("plan hash: "))
        .unwrap()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_is_a_dry_run_by_default_and_idempotent_when_applied() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let before = env.site_file();
    let out = env.ok(&["install-app", "sample"]);
    assert!(out.contains("plan hash: "), "{out}");
    assert!(out.contains("[additive]"), "{out}");
    assert!(out.contains("dry run"), "{out}");
    assert_eq!(before, env.site_file(), "a dry run must not touch the site");
    assert!(!env.live().await.tables.contains_key("task"));

    let out = env.ok(&["install-app", "sample", "--apply", "--seed"]);
    assert!(out.contains("installed app `sample`"), "{out}");
    assert!(out.contains("seeded 21 row(s); 0 already present"), "{out}");
    assert!(
        env.site_file()
            .contains("apps = [\"platform\", \"sample\"]")
    );
    assert!(env.live().await.tables["task"].has_rows);
    assert!(
        env.ok(&["list-apps", "--site", &env.site])
            .contains("sample 1.0.0")
    );

    let out = env.ok(&["migrate", "--site", &env.site]);
    assert!(out.contains("up to date"), "{out}");
    let out = env.ok(&["migrate", "--site", &env.site, "--apply"]);
    assert!(out.contains("up to date"), "{out}");

    let out = env.ok(&["install-app", "sample", "--apply", "--seed"]);
    assert!(out.contains("already installed"), "{out}");
    assert!(out.contains("seeded 0 row(s); 21 already present"), "{out}");
    let out = env.ok(&["install-app", "sample", "--apply"]);
    assert!(out.contains("already installed"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_field_is_an_additive_plan_and_applying_it_keeps_the_data() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    env.ok(&["install-app", "sample", "--apply", "--seed"]);
    let path = env.sample("task.toml");
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{original}{NEW_FIELD}")).unwrap();

    let out = env.ok(&["migrate", "--site", &env.site, "--dry-run"]);
    assert!(
        out.contains("[additive]") && out.contains("review_note"),
        "{out}"
    );
    assert!(
        !env.live().await.tables["task"]
            .columns
            .contains_key("review_note")
    );

    // Install of an installed app with a changed DocType points at the same plan and applies.
    let out = env.ok(&["install-app", "sample", "--apply"]);
    assert!(out.contains("applied"), "{out}");
    let live = env.live().await;
    assert!(live.tables["task"].columns.contains_key("review_note"));
    assert!(live.tables["task"].has_rows, "the six tasks must survive");
    let out = env.ok(&["migrate", "--site", &env.site]);
    assert!(out.contains("up to date"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_change_exits_non_zero_and_changes_nothing() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    env.ok(&["install-app", "sample", "--apply"]);
    let path = env.sample("task.toml");
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, without_field(&original, "estimated_hours")).unwrap();
    let before = env.live().await;
    for args in [
        vec!["migrate", "--site", &env.site],
        vec!["migrate", "--site", &env.site, "--apply"],
    ] {
        let output = env.spot(&args);
        let out = text(&output);
        assert!(!output.status.success(), "{out}");
        assert!(
            out.contains("[refused]") && out.contains("estimated_hours"),
            "{out}"
        );
    }
    let after = env.live().await;
    assert_eq!(
        before.tables["task"].columns.len(),
        after.tables["task"].columns.len()
    );
    assert!(after.tables["task"].columns.contains_key("estimated_hours"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plan_hash_for_another_plan_refuses_to_apply() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    env.ok(&["install-app", "sample", "--apply"]);
    let path = env.sample("label.toml");
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{original}{NEW_FIELD}")).unwrap();
    let reviewed = plan_hash(&env.ok(&["migrate", "--site", &env.site]));

    let wrong = "0".repeat(64);
    let output = env.spot(&[
        "migrate",
        "--site",
        &env.site,
        "--apply",
        "--plan-hash",
        &wrong,
    ]);
    let out = text(&output);
    assert!(!output.status.success(), "{out}");
    assert!(out.contains("changed since it was reviewed"), "{out}");
    assert!(
        !env.live().await.tables["label"]
            .columns
            .contains_key("review_note")
    );

    let out = env.ok(&[
        "migrate",
        "--site",
        &env.site,
        "--apply",
        "--plan-hash",
        &reviewed,
    ]);
    assert!(out.contains("applied"), "{out}");
    assert!(
        env.live().await.tables["label"]
            .columns
            .contains_key("review_note")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_lists_every_metadata_error_and_changes_nothing() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let before = env.site_file();
    std::fs::write(env.sample("label.toml"), "name = [broken").unwrap();
    std::fs::write(env.sample("project.toml"), "not a doctype = true").unwrap();
    std::fs::write(
        env.root.join("sample/app.toml"),
        "[app]\nname = \"Not Valid\"\n",
    )
    .unwrap();
    let output = env.spot(&["install-app", "sample", "--apply"]);
    let out = text(&output);
    assert!(!output.status.success(), "{out}");
    for expected in ["label.toml", "project.toml", "app.toml"] {
        assert!(out.contains(expected), "{expected} missing from: {out}");
    }
    assert_eq!(before, env.site_file());
    assert!(!env.live().await.tables.contains_key("task"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_password_rejects_an_argument_and_prints_every_violated_rule() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    env.ok(&["add-user", &env.site, "bobby@example.com"]);

    let output = env.spot(&[
        "set-password",
        &env.site,
        "bobby@example.com",
        "--password",
        "hunter2-secret",
    ]);
    let out = text(&output);
    assert!(!output.status.success());
    assert!(!out.contains("hunter2-secret"), "{out}");
    let output = env.spot(&[
        "set-password",
        &env.site,
        "bobby@example.com",
        "hunter2-secret",
    ]);
    assert!(!output.status.success());
    // The Administrator's and the invited user's one-time secrets; the refused calls added none.
    assert_eq!(env.site_file().matches("[[credentials]]").count(), 2);

    let output = run(
        &env.root,
        &[
            "set-password",
            &env.site,
            "bobby@example.com",
            "--password-stdin",
        ],
        Some("bobby\n"),
    );
    let out = text(&output);
    assert!(!output.status.success(), "{out}");
    assert!(
        out.contains("too_short") && out.contains("too_weak"),
        "{out}"
    );
    assert!(
        !out.contains("bobby\n"),
        "the password must not be echoed: {out}"
    );

    let output = run(
        &env.root,
        &[
            "set-password",
            &env.site,
            "bobby@example.com",
            "--password-stdin",
        ],
        Some("bobby-correct-horse-battery\n"),
    );
    let out = text(&output);
    assert!(
        !output.status.success(),
        "the e-mail's own name is context: {out}"
    );
    assert!(out.contains("contains_context"), "{out}");

    let strong = "violet-lantern-staple-9-Quartz";
    let output = run(
        &env.root,
        &[
            "set-password",
            &env.site,
            "bobby@example.com",
            "--password-stdin",
        ],
        Some(&format!("{strong}\n")),
    );
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(!out.contains(strong));
    let site = env.site_file();
    assert!(site.contains("$argon2id$") && !site.contains(strong));

    let output = run(
        &env.root,
        &[
            "set-password",
            &env.site,
            "nobody@example.com",
            "--password-stdin",
        ],
        Some(&format!("{strong}\n")),
    );
    assert!(!output.status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_user_prints_the_secret_once_and_stores_only_its_digest() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let out = env.ok(&[
        "add-user",
        &env.site,
        "ama@example.com",
        "--name",
        "Ama",
        "--role",
        "User Manager",
    ]);
    let secret = out.trim().rsplit(' ').next().unwrap().to_owned();
    assert!(secret.starts_with("spk_"), "{out}");
    assert_eq!(out.matches(&secret).count(), 1);
    assert_eq!(out.lines().count(), 1);
    let site = env.site_file();
    assert!(!site.contains(&secret));
    // The invitation is a one-time sign-in verifier with an expiry, never an API key.
    let key_id = secret.split('_').nth(1).unwrap();
    assert!(
        !site.contains(key_id),
        "the secret is not an API key verifier"
    );
    assert!(!site.contains("[[api_keys]]"), "{site}");
    assert_eq!(site.matches("first_sign_in = true").count(), 2, "{site}");
    assert_eq!(site.matches("expires_at = ").count(), 2, "{site}");

    for later in [
        env.ok(&["list-apps"]),
        env.ok(&["check"]),
        env.ok(&["migrate"]),
    ] {
        assert!(!later.contains(&secret));
    }
    let output = env.spot(&["add-user", &env.site, "AMA@example.com"]);
    let out = text(&output);
    assert!(
        !output.status.success() && out.contains("already exists"),
        "{out}"
    );
    assert!(!out.contains("spk_"));

    // A refused request writes nothing: the same e-mail works afterwards.
    let output = env.spot(&[
        "add-user",
        &env.site,
        "eli@example.com",
        "--role",
        "No Such Role",
    ]);
    assert!(!output.status.success() && text(&output).contains("unknown role"));
    assert_eq!(env.site_file().matches("[[credentials]]").count(), 2);
    env.ok(&[
        "add-user",
        &env.site,
        "eli@example.com",
        "--role",
        "System Manager",
    ]);
    assert_eq!(env.site_file().matches("[[credentials]]").count(), 3);

    let output = env.spot(&["add-user", &env.site, "not-an-email"]);
    assert!(!output.status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_admin_needs_the_flag_replaces_the_credentials_and_leaves_an_audit_record() {
    let Some(env) = Env::new().await else { return };
    let first = env.bare_site();
    let first_id = first.split('_').nth(1).unwrap().to_owned();
    let before = env.site_file();
    assert!(!before.contains(&first_id), "the secret is not stored");
    let first_hash = before
        .lines()
        .find(|l| l.starts_with("password_hash"))
        .unwrap()
        .to_owned();

    let output = env.spot(&["reset-admin", &env.site]);
    let out = text(&output);
    assert!(
        !output.status.success() && out.contains("--yes-i-am-the-operator"),
        "{out}"
    );
    assert_eq!(before, env.site_file());
    // Recorded security finding: the refusal is audited (and only the refusal: nothing was changed).
    let refused =
        std::fs::read_to_string(env.root.join("sites").join(&env.site).join("audit.jsonl"))
            .unwrap();
    assert_eq!(refused.lines().count(), 1, "{refused}");
    assert!(
        refused.contains("\"outcome\":\"refused\"")
            && refused.contains("operator_confirmation_missing")
            && !refused.contains("\"revoked\""),
        "{refused}"
    );

    let out = env.ok(&["reset-admin", &env.site, "--yes-i-am-the-operator"]);
    let secret = out.trim().rsplit(' ').next().unwrap();
    assert!(secret.starts_with("spk_") && !secret.contains(&first_id));
    let after = env.site_file();
    assert!(
        !after.contains(&first_hash),
        "the old Administrator verifier must stop working"
    );
    assert!(!after.contains(secret));
    let audit = std::fs::read_to_string(env.root.join("sites").join(&env.site).join("audit.jsonl"))
        .unwrap();
    assert!(
        audit.contains("\"action\":\"reset-admin\"") && audit.contains("\"revoked\":1"),
        "{audit}"
    );
    assert!(!audit.contains(secret));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(env.root.join("sites").join(&env.site).join("audit.jsonl"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_reports_a_bad_connection_and_a_malformed_site_together() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    assert!(env.ok(&["check"]).starts_with("ok: 1 site"));

    let sites = env.root.join("sites");
    std::fs::create_dir_all(sites.join("ghost")).unwrap();
    std::fs::write(
        sites.join("ghost/site.toml"),
        "site = 'ghost'\ndatabase = 'spot_ghost_missing'\napps = ['platform']\n",
    )
    .unwrap();
    std::fs::create_dir_all(sites.join("broken")).unwrap();
    std::fs::write(
        sites.join("broken/site.toml"),
        "site = 'broken'\ndatabase = 'broken'\nnot_a_field = 1\n",
    )
    .unwrap();
    let output = env.spot(&["check"]);
    let out = text(&output);
    assert!(!output.status.success(), "{out}");
    assert!(out.contains("site `ghost`: cannot reach"), "{out}");
    assert!(
        out.contains("broken/site.toml") && out.contains("not_a_field"),
        "{out}"
    );
    assert!(
        !out.contains(&env.url),
        "the admin URL must never be printed"
    );
}

//! WP-PLT-11.T10: the site host registers the mail sender's final-attempt hook with the jobs
//! service, so a mail job killed on its last attempt bounces its recipients in production.
//!
//! `tests/mail_bounce.rs` (WP-PLT-11.T9) proves the worker and the hook once they are wired; this
//! file guards the wiring itself, in `src/host.rs`, which no seam of the crate lets a test
//! drive without a database and a mail job that really outlives its queue's timeout (the queue
//! policy is not configurable from `common.toml`). The guard therefore reads the host's source:
//! it fails if the registration is removed, duplicated, or moved off the jobs service the host
//! adds. See the task report for the seam a behavioural test would need.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

const HOST: &str = include_str!("../src/services/jobs.rs");
const MAIL: &str = include_str!("../src/services/mail.rs");
const REGISTRATION: &str = "jobs.register_final_attempt_hook(mail.final_attempt_hook());";
const BUILD: &str = "Jobs::new(&settings, handlers)";

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[test]
fn the_host_registers_the_mail_hook_once_on_the_jobs_it_adds() {
    assert_eq!(
        count(HOST, REGISTRATION),
        1,
        "the host must register the mail final-attempt hook exactly once per site's Jobs"
    );
    let registration = HOST.find(REGISTRATION).unwrap();
    let build = HOST[..registration]
        .rfind(BUILD)
        .expect("the hook is registered on the Jobs the host builds, not on another value");
    let added = HOST[registration..]
        .find("ctx.add(jobs);")
        .expect("the registered Jobs is the one added to the site's services");
    assert!(
        registration - build < 1500 && added < 400,
        "the registration sits between building the Jobs and adding it"
    );
}

#[test]
fn the_mail_module_provides_the_mail_the_jobs_module_reads() {
    assert_eq!(count(MAIL, "ctx.provide::<Mail>(mail)?;"), 1);
    assert_eq!(
        count(HOST, "ctx.port::<crate::services::mail::Mail>()"),
        1,
        "the jobs module reads the one site's Mail from the ports map"
    );
}

#[test]
fn nothing_else_registers_a_final_attempt_hook_in_the_host() {
    // Its definition is `pub fn register_final_attempt_hook`, so the module has two mentions.
    assert_eq!(count(HOST, "jobs.register_final_attempt_hook("), 1);
}

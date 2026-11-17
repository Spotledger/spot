# spot-identity-conformance

The conformance suite of ADR-083 (Appendix A), cut to what exists once WP-IAM-11 tasks 1 to 4 have
merged: B1 (audience), B2 (listener), B3 (outside users), B5 (audience does not move) and B7 (defaults).
B4, B6, B8 and B9 are `#[ignore]`d tests that name the task that owns them; run
`cargo test -p spot-identity-conformance -- --ignored --list` to see what is outstanding.

**Where it lives.** The ADR's Appendix A names `crates/framework/spot-identity-conformance/**`.
This crate is in `harness/rust/` instead (the workspace already globs `harness/rust/*`): it is test
code with many dev-dependencies, and no production crate may depend on it. This is a deliberate
change from the ADR, made by the task brief.

**What it drives.** The real router of each audience (`router_for`) over the fake application services
of `spot-http`'s own test support (included by path) and the real `LoginService` over in-memory ports,
with real sessions.

`consumer-surface.snapshot` pins the consumer surface: one `METHOD /path` line per route marked for
the consumer audience, sorted. It is empty today (no route is marked consumer). Adding a consumer
route needs a visible diff of this file.

//! L7 invariant suite, framework cut, part 2 (WP-TST-07.T2): the storage-level driver of
//! `spot-l7-invariants` (WP-TST-07.T1) run through the **document engine** and the **job
//! engine**.
//!
//! Part 1 proved the storage promises under parallel load. This part proves the platform's own
//! promises one level up, on the sample app's DocTypes (WP-DX-05.T1) with no business scenario:
//!
//! - the engine allocates a document's name, runs the lifecycle and writes the outbox event
//!   inside one transaction, so naming is gapless, states follow the ADR-42 machine, and every
//!   committed operation produces exactly its one lifecycle event, once, with the right actor;
//! - permissions are checked per operation, so a refusal leaves no effect at all;
//! - the job engine retries, fences and deduplicates idempotent jobs without losing one;
//! - an app can register its own invariants next to the framework's, and one that panics or
//!   runs long is reported as a failed invariant, never as a crash of the run.
//!
//! The invariants are T1's [`Invariant`](spot_l7_invariants::Invariant)s, registered on T1's
//! [`InvariantRegistry`](spot_l7_invariants::InvariantRegistry) and checked through T1's
//! [`CheckContext`](spot_l7_invariants::CheckContext); the engine facts they need beyond the
//! store travel in each invariant's own state.

pub mod app;
pub mod corrupt;
pub mod driver;
pub mod invariants;
pub mod jobs;
pub mod naming;
pub mod oracle;
pub mod runner;
pub mod world;

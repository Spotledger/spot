//! Customer-signed, hardware-bound deep support access (WP-SEC-07.T1).
//!
//! Requests convey no authority. The sole credential constructor verifies the customer's
//! exact consent, the hardware key and the receipt nonce before writing issuance to the
//! tenant audit chain. The platform supplies all clocks, entropy, authoritative stores,
//! action catalogue and hard-floor evidence. Operators never supply these trusted inputs.
//!
//! Scope uses spot-support-session; agent hard-floor checks use spot-agent-tiers;
//! authentication methods use spot-iam-access; key identity uses spot-signed-command.
//! The two existing tier enums are explicitly mapped, without modifying either crate.
mod attestation;
mod error;
mod issue;
mod log;
mod model;
mod state;
mod usage;
pub use attestation::{KeyAttestation, OperatorAttestation, VendorAttestation};
pub use error::DeepError;
pub use issue::{IssueContext, escalate, issue};
pub use log::{AuditExecutor, ChainLog, DeepAccessEvent, SessionLog, SessionReport};
pub use model::{
    AccessRequest, ConsentReceipt, CustomerTrust, DeepCredential, Hash, Policy, TrustedApprover,
};
pub use state::{
    MemoryNonces, MemoryRevocations, ReceiptNonces, Revocation, RevocationStore, RevocationView,
    UseState,
};
pub use usage::{ActionRequest, Decision, UseContext, UseProof, authorise, revoke};

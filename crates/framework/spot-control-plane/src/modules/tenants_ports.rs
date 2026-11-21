//! The ports and adapters of the `tenants` module (`WP-CTL-01.T7`): signed approvals, the audit bridge, the invitation queue, telemetry registration, the licence store and the provisioning-runner seam. Split from `tenants.rs` for the 1,000-line gate.

use super::tenants_kv::Kv;
use crate::CpCode;
use crate::CpError;
use crate::auth::sha256_hex;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;
use spot_mgmt_api::ports::AuditOutcome;
use spot_mgmt_api::ports::AuditRecord;
use spot_mgmt_api::ports::AuditSink as MgmtAudit;
use spot_mgmt_api::ports::BoxFuture;
use spot_mgmt_api::wire;
use spot_signed_command::CommandVerifier;
use spot_signed_command::KeyId;
use spot_signed_command::NonceStore;
use spot_signed_command::TrustedKeys;
use spot_signed_command::VerifyPolicy;
use spot_tenancy::Tenant;
use spot_tenancy::TenantId;
use spot_tenancy::ports::ApprovalHook;
use spot_tenancy::ports::AuditEvent;
use spot_tenancy::ports::AuditSink as PlaneAudit;
use spot_tenancy::ports::Outcome;
use spot_tenancy::ports::PortResult;
use spot_tenancy_adapters::AdapterError;
use spot_tenancy_adapters::LicenceStore;
use spot_tenancy_adapters::MigrationRunner;
use spot_tenancy_adapters::TelemetryRegistry;
use spot_tenancy_adapters::UserProvisioning;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------------------------
// Ports of this module
// ---------------------------------------------------------------------------------------------

/// Verifies the signed approval of a deprovisioning; answers the approver identities.
pub trait ApprovalVerifier: Send + Sync {
    /// The approvers (signing key ids) when `approval` is a valid, fresh signed command that binds
    /// `"<tenant>:<request id>"`.
    fn verify<'a>(
        &'a self,
        approval: &'a str,
        tenant: &'a str,
        request_id: &'a str,
        now: i64,
    ) -> BoxFuture<'a, Result<Vec<String>, CpError>>;
}

/// [`ApprovalVerifier`] over the administrator keys and a durable nonce store.
pub struct SignedApprovals {
    verifier: CommandVerifier,
    keys: wire::KeyDirectory,
    audience: String,
}

impl SignedApprovals {
    /// Trusts `administrator_keys`; the approval must name `audience`.
    #[must_use]
    pub fn new(
        administrator_keys: &[[u8; 32]],
        nonces: Box<dyn NonceStore>,
        audience: &str,
    ) -> Self {
        let mut trusted = TrustedKeys::new();
        let mut keys: wire::KeyDirectory = BTreeMap::new();
        for key in administrator_keys {
            let id = KeyId::from_public_key(key);
            if trusted.insert(id.clone(), key).is_ok() {
                keys.insert(id.as_str().to_owned(), *key);
            }
        }
        let policy = VerifyPolicy {
            max_skew: spot_mgmt_api::MAX_CLOCK_SKEW_SECS,
            max_lifetime: spot_mgmt_api::MAX_COMMAND_LIFETIME_SECS,
            min_signatures: 1,
        };
        Self {
            verifier: CommandVerifier::new(trusted, policy, nonces),
            keys,
            audience: audience.to_owned(),
        }
    }
}

impl ApprovalVerifier for SignedApprovals {
    fn verify<'a>(
        &'a self,
        approval: &'a str,
        tenant: &'a str,
        request_id: &'a str,
        now: i64,
    ) -> BoxFuture<'a, Result<Vec<String>, CpError>> {
        Box::pin(async move {
            let invalid = || CpError::with_reason(CpCode::SignatureInvalid, "cp.approval_invalid");
            let raw = URL_SAFE_NO_PAD
                .decode(approval.as_bytes())
                .map_err(|_| invalid())?;
            let signed = wire::decode(&raw)
                .map_err(|_| invalid())?
                .to_signed(&self.keys)
                .map_err(|_| invalid())?;
            let verified = self
                .verifier
                .verify(&signed, &self.audience, now)
                .await
                .map_err(|error| match error {
                    spot_signed_command::CommandError::Replay => {
                        CpError::with_reason(CpCode::Replayed, "cp.replayed")
                    }
                    spot_signed_command::CommandError::Store(_)
                    | spot_signed_command::CommandError::InvalidKey => {
                        CpError::with_reason(CpCode::Internal, "cp.nonce_store_unavailable")
                    }
                    _ => invalid(),
                })?;
            let params = verified.command.params();
            let digest = sha256_hex(format!("{tenant}:{request_id}").as_bytes());
            let path = format!("/cp/v1/tenants/{tenant}/deprovision/confirm");
            let binds = verified.command.op() == "cp.cp_tenant_deprovision_approve"
                && params.get("body_sha256").and_then(Value::as_str) == Some(digest.as_str())
                && params.get("path").and_then(Value::as_str) == Some(path.as_str());
            if !binds || verified.signers.is_empty() {
                return Err(CpError::with_reason(
                    CpCode::SignatureInvalid,
                    "cp.approval_unbound",
                ));
            }
            Ok(verified
                .signers
                .iter()
                .map(|id| format!("key:{}", id.as_str()))
                .collect())
        })
    }
}

/// The approvers of one deletion, fixed by the confirmation that was verified.
pub(crate) struct FixedApprovals(pub(crate) Vec<String>);

impl ApprovalHook for FixedApprovals {
    fn approvers(&self, _tenant: &Tenant) -> Vec<String> {
        self.0.clone()
    }
}

/// Runs a future to completion on the module's own runtime from a blocking thread.
pub(crate) fn block_on<T>(runtime: &tokio::runtime::Runtime, future: impl Future<Output = T>) -> T {
    runtime.block_on(future)
}

/// A runtime for the synchronous ports. It is never dropped (dropping a runtime inside another
/// runtime's context panics), so it lives as long as the process.
#[must_use]
pub fn leaked_runtime() -> Arc<tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<Arc<tokio::runtime::Runtime>> = OnceLock::new();
    Arc::clone(RUNTIME.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("cp-tenants")
            .build();
        match runtime {
            Ok(rt) => Arc::new(rt),
            Err(_) => {
                Arc::new(tokio::runtime::Runtime::new().unwrap_or_else(|_| std::process::abort()))
            }
        }
    }))
}

/// The audit port of the plane over the control plane's audit sink.
pub(crate) struct AuditBridge {
    pub(crate) audit: Arc<dyn MgmtAudit>,
    pub(crate) runtime: Arc<tokio::runtime::Runtime>,
}

impl PlaneAudit for AuditBridge {
    fn record(&self, event: &AuditEvent) -> PortResult {
        let (outcome, reason) = match event.outcome {
            Outcome::Success => (AuditOutcome::Succeeded, None),
            Outcome::Failure(code) => (AuditOutcome::Failed, Some(code.to_owned())),
        };
        let action = match event.step {
            Some(step) => format!("tenancy.{}.{step}:{}", event.action, event.tenant),
            None => format!("tenancy.{}:{}", event.action, event.tenant),
        };
        let record = AuditRecord {
            request_id: format!("tenancy-{}-{}", event.action, event.at),
            principal: Some(event.actor.clone()),
            certificate_fingerprint: None,
            action,
            operation: None,
            risk: None,
            outcome,
            reason,
            command_id: None,
            at: i64::try_from(event.at / 1000).unwrap_or(0),
        };
        block_on(&self.runtime, self.audit.record(record))
            .map(|_| ())
            .map_err(|_| spot_tenancy::ports::StepFailure {
                retryable: true,
                reason: "tenants.audit_unavailable",
            })
    }
}

/// The IAM source: queues the administrator invitation (the mail itself is sent elsewhere); the
/// address was stored when the tenant was requested. No credential is created or kept here.
pub(crate) struct InviteQueue {
    pub(crate) kv: Arc<dyn Kv>,
    pub(crate) runtime: Arc<tokio::runtime::Runtime>,
}

impl UserProvisioning for InviteQueue {
    fn create_users(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        let id = tenant.id.to_string();
        let found =
            block_on(&self.runtime, self.kv.get("invite", &id)).map_err(|_| AdapterError::Iam)?;
        if found.is_none() {
            return Err(AdapterError::Iam);
        }
        block_on(&self.runtime, self.kv.put("invite_queued", &id, "1"))
            .map_err(|_| AdapterError::Iam)
    }
    fn remove_users(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        let id = tenant.id.to_string();
        for ns in ["invite_queued", "invite"] {
            block_on(&self.runtime, self.kv.del(ns, &id)).map_err(|_| AdapterError::Iam)?;
        }
        Ok(())
    }
}

/// Telemetry registration: one control-schema row per tenant.
pub(crate) struct KvTelemetry {
    pub(crate) kv: Arc<dyn Kv>,
    pub(crate) runtime: Arc<tokio::runtime::Runtime>,
}

impl TelemetryRegistry for KvTelemetry {
    fn register(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        block_on(
            &self.runtime,
            self.kv.put("telemetry", &tenant.id.to_string(), "1"),
        )
        .map_err(|_| AdapterError::Telemetry)
    }
    fn deregister(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        block_on(
            &self.runtime,
            self.kv.del("telemetry", &tenant.id.to_string()),
        )
        .map_err(|_| AdapterError::Telemetry)
    }
}

/// The issued licences, in the control schema (`licence_active`, `licence_serial`): the rows the
/// `licensing` module serves to hosts.
pub struct KvLicenceStore {
    kv: Arc<dyn Kv>,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl KvLicenceStore {
    /// A store over `kv`.
    #[must_use]
    pub fn new(kv: Arc<dyn Kv>, runtime: Arc<tokio::runtime::Runtime>) -> Self {
        Self { kv, runtime }
    }
}

impl LicenceStore for KvLicenceStore {
    fn active(
        &self,
        tenant: &TenantId,
    ) -> Result<Option<spot_license::SignedLicence>, AdapterError> {
        let text = block_on(
            &self.runtime,
            self.kv.get("licence_active", &tenant.to_string()),
        )
        .map_err(|_| AdapterError::LicenceStore)?;
        text.map(|t| {
            spot_license::SignedLicence::from_text(&t).map_err(|_| AdapterError::LicenceStore)
        })
        .transpose()
    }
    fn save(
        &self,
        tenant: &TenantId,
        licence: &spot_license::SignedLicence,
        serial: u64,
    ) -> Result<(), AdapterError> {
        let text = licence.to_text().map_err(|_| AdapterError::LicenceStore)?;
        let id = tenant.to_string();
        block_on(&self.runtime, self.kv.put("licence_active", &id, &text))
            .map_err(|_| AdapterError::LicenceStore)?;
        let high = self.highest_serial(tenant)?.max(serial);
        block_on(
            &self.runtime,
            self.kv.put("licence_serial", &id, &high.to_string()),
        )
        .map_err(|_| AdapterError::LicenceStore)
    }
    fn revoke(&self, tenant: &TenantId) -> Result<(), AdapterError> {
        block_on(
            &self.runtime,
            self.kv.del("licence_active", &tenant.to_string()),
        )
        .map_err(|_| AdapterError::LicenceStore)
    }
    fn highest_serial(&self, tenant: &TenantId) -> Result<u64, AdapterError> {
        let text = block_on(
            &self.runtime,
            self.kv.get("licence_serial", &tenant.to_string()),
        )
        .map_err(|_| AdapterError::LicenceStore)?;
        Ok(text.and_then(|t| t.parse().ok()).unwrap_or(0))
    }
}

/// Shares one runner between the schema and the seed adapter.
pub(crate) struct SharedRunner(pub(crate) Arc<dyn MigrationRunner>);

impl MigrationRunner for SharedRunner {
    fn ensure_schema(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        self.0.ensure_schema(tenant)
    }
    fn drop_schema(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        self.0.drop_schema(tenant)
    }
    fn apply_baseline(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        self.0.apply_baseline(tenant)
    }
    fn seed(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        self.0.seed(tenant)
    }
    fn unseed(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        self.0.unseed(tenant)
    }
}

/// What a [`RunnerFactory`] builds the provisioning executor from.
pub struct ProvisionerInputs {
    /// The provisioning role's connection settings (it may only create databases).
    pub admin: spot_postgres::PgConfig,
    /// The placement databases.
    pub placements: Vec<spot_tenancy_pg::PlacementDb>,
    /// The runtime the synchronous ports block on.
    pub runtime: Arc<tokio::runtime::Runtime>,
}

/// Builds the `spot_tenancy_pg::PgProvisioner` (it needs the application catalogue, which the
/// control plane must not depend on) and returns it as a [`MigrationRunner`].
pub type RunnerFactory = fn(ProvisionerInputs) -> Result<Arc<dyn MigrationRunner>, CpError>;

pub(crate) static RUNNER_FACTORY: OnceLock<RunnerFactory> = OnceLock::new();

/// Registers the factory the production wiring builds the provisioning executor with; `false`
/// when one is already registered.
pub fn set_runner_factory(factory: RunnerFactory) -> bool {
    RUNNER_FACTORY.set(factory).is_ok()
}

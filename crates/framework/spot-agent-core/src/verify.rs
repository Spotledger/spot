//! The shared evaluation: the one function both the broker and the executor run.
//!
//! [`Evaluator::evaluate`] is the verification chain of the brief, in order: resolve the
//! operation and its risk in the catalogue; enforce the risk's signature count through the
//! signed-command verifier against the trust store; apply the requester and hardware-key
//! rules; validate the arguments against the catalogue schema; clear a critical operation;
//! and build the [`ExecutionPlan`].
//!
//! The broker passes the live nonce store (`Some`), so replay protection is real. The
//! executor passes `None`: it re-derives the plan from a command the broker already
//! replay-checked, and must not burn the nonce twice. Every other check runs identically,
//! which is what makes "the plan is the deterministic plan of a validly signed command" a
//! statement the executor can verify on its own.

use std::sync::Arc;

use serde_json::Value;
use spot_mgmt_ops::{Catalogue, Invocation, OpId, OperationSpec, ParamType, ParamValue};
use spot_signed_command::{CommandVerifier, NonceFuture, NonceStore, SignedCommand, StoreError};

use crate::error::Refusal;
use crate::plan::{ExecutionPlan, command_digest};
use crate::policy::{CriticalContext, CriticalRequest, Policy};
use crate::trust::TrustStore;

/// The shared verification state: everything a decision depends on, borrowed so the same
/// code serves the unprivileged broker and the privileged executor.
pub(crate) struct Evaluator<'a> {
    /// The agent the command must be addressed to.
    pub agent_id: &'a str,
    /// The operation catalogue.
    pub catalogue: &'a Catalogue,
    /// The administrator trust store.
    pub trust: &'a TrustStore,
    /// The decision policy.
    pub policy: &'a Policy,
    /// The critical-operation clearance boundary.
    pub critical: &'a dyn CriticalContext,
}

impl Evaluator<'_> {
    /// Runs the chain and returns a plan, or the first refusal.
    ///
    /// # Errors
    /// Every [`Refusal`] the checks produce.
    pub(crate) async fn evaluate(
        &self,
        signed: &SignedCommand,
        nonce: Option<&Arc<dyn NonceStore>>,
        now: i64,
    ) -> Result<ExecutionPlan, Refusal> {
        let command = &signed.command;

        // The operation is resolved first so the risk level — and therefore the number of
        // required signatures — comes from the catalogue, never from the envelope.
        let op = OpId::new(command.op()).ok_or(Refusal::UnknownOperation)?;
        let spec = self.catalogue.get(&op).ok_or(Refusal::UnknownOperation)?;
        let risk = spec.risk;
        let required = Policy::required_distinct(risk);

        // Signature count (distinct trusted keys), audience, freshness window, lifetime
        // bound and — for the broker — the replay nonce, enforced by the verifier.
        let verifier = match nonce {
            Some(store) => CommandVerifier::new(
                self.trust.trusted_keys(),
                self.policy.verify_policy(required),
                Box::new(SharedNonce(store.clone())),
            ),
            None => CommandVerifier::new(
                self.trust.trusted_keys(),
                self.policy.verify_policy(required),
                Box::new(SkipNonce),
            ),
        };
        let verified = verifier
            .verify(signed, self.agent_id, now)
            .await
            .map_err(Refusal::from)?;
        let signers = verified.signers;

        // Two-person rule: when a second administrator is required, the requester may not
        // be one of the approvers (their own approval proves nothing).
        if required >= 2
            && self.policy.requester_cannot_approve
            && signers.iter().any(|signer| signer == command.issuer())
        {
            return Err(Refusal::RequesterCannotApprove {
                requester: command.issuer().as_str().to_owned(),
            });
        }

        // Administrator signatures are hardware-key signatures.
        if self.policy.require_hardware_keys {
            for signer in &signers {
                if !self.trust.is_hardware(signer) {
                    return Err(Refusal::SoftwareKeyRefused {
                        key_id: signer.as_str().to_owned(),
                    });
                }
            }
        }

        // Arguments against the operation's schema. The detail uses the stable code and
        // the parameter name only, never a value (a parameter may be secret).
        let invocation = invocation_from_json(spec, command.params())?;
        let args = self.catalogue.validate(&invocation).map_err(|errors| {
            let detail = errors.first().map_or_else(
                || "the invocation is invalid".to_owned(),
                |error| match error.param() {
                    Some(param) => format!("{} on parameter {}", error.code(), param),
                    None => error.code().to_owned(),
                },
            );
            Refusal::InvalidArguments {
                op: op.as_str().to_owned(),
                detail,
            }
        })?;

        // Critical operations need a configured delay or a break-glass clearance.
        let digest = command_digest(command)?;
        if Policy::is_critical(risk) {
            let request = CriticalRequest {
                agent_id: self.agent_id,
                command_id: command.id(),
                op: &op,
                command_digest: digest,
                requester: command.issuer(),
                issued_at: command.issued_at(),
            };
            self.critical.clearance(&request, now)?;
        }

        Ok(ExecutionPlan::new(crate::plan::PlanParts {
            agent_id: self.agent_id.to_owned(),
            command_id: command.id().to_owned(),
            command_digest: digest,
            op,
            args,
            risk,
            signers,
            issued_at: command.issued_at(),
        }))
    }
}

/// Converts the command's free JSON parameters into a typed [`Invocation`] using the
/// operation's schema, then lets the catalogue do the real validation.
///
/// # Errors
/// [`Refusal::InvalidArguments`] when `params` is not a JSON object or a declared
/// parameter's value does not fit its declared type.
fn invocation_from_json(spec: &OperationSpec, params: &Value) -> Result<Invocation, Refusal> {
    let Some(object) = params.as_object() else {
        return Err(Refusal::InvalidArguments {
            op: spec.id.as_str().to_owned(),
            detail: "the parameters must be a JSON object".to_owned(),
        });
    };
    let mut invocation = Invocation::new(spec.id.clone());
    for (name, value) in object {
        match spec.params.iter().find(|param| param.name == *name) {
            Some(param) => {
                let converted =
                    convert(&param.ty, value).ok_or_else(|| Refusal::InvalidArguments {
                        op: spec.id.as_str().to_owned(),
                        detail: format!("parameter {name} has a value of the wrong type"),
                    })?;
                invocation.params.insert(name.clone(), converted);
            }
            None => {
                // Unknown parameter: keep a value so the catalogue reports it as unknown
                // (the name is the caller's, so it is never echoed into a message).
                invocation.params.insert(name.clone(), best_effort(value));
            }
        }
    }
    Ok(invocation)
}

/// Converts one JSON value to the declared parameter type, or `None` on a shape mismatch.
fn convert(ty: &ParamType, value: &Value) -> Option<ParamValue> {
    match ty {
        ParamType::Bool => value.as_bool().map(ParamValue::Bool),
        ParamType::Int { .. } => value.as_i64().map(ParamValue::Int),
        ParamType::Duration => value.as_u64().map(ParamValue::Duration),
        ParamType::Text { .. } | ParamType::Enum(_) => {
            value.as_str().map(|text| ParamValue::Text(text.to_owned()))
        }
        ParamType::List(inner) => {
            let items = value.as_array()?;
            let mut converted = Vec::with_capacity(items.len());
            for item in items {
                converted.push(convert(inner, item)?);
            }
            Some(ParamValue::List(converted))
        }
    }
}

/// A type-agnostic value for an unknown parameter, so the catalogue can report it. The
/// shape carries no meaning; only the parameter name matters for the unknown-parameter
/// check.
fn best_effort(value: &Value) -> ParamValue {
    if let Some(flag) = value.as_bool() {
        ParamValue::Bool(flag)
    } else if let Some(number) = value.as_i64() {
        ParamValue::Int(number)
    } else if let Some(text) = value.as_str() {
        ParamValue::Text(text.to_owned())
    } else if let Some(items) = value.as_array() {
        ParamValue::List(items.iter().map(best_effort).collect())
    } else {
        ParamValue::Text(value.to_string())
    }
}

/// The nonce store adapter the broker uses: a shared store behind an `Arc`, owned by the
/// verifier for the duration of one decision.
struct SharedNonce(Arc<dyn NonceStore>);

impl NonceStore for SharedNonce {
    fn check_and_insert<'a>(
        &'a self,
        key: &'a spot_signed_command::KeyId,
        nonce: [u8; 16],
        expires_at: i64,
        now: i64,
    ) -> NonceFuture<'a, Result<bool, StoreError>> {
        self.0.check_and_insert(key, nonce, expires_at, now)
    }
}

/// The no-op nonce store the executor uses: it re-derives a plan from a command the broker
/// already replay-checked and must not consume the nonce a second time. It records nothing.
///
/// This is the only place on the path where `Ok(true)` is not a store's answer: it is
/// allowed because the broker already spent the nonce in its durable store before it signed
/// the plan, and the executor accepts only plans signed by that broker.
struct SkipNonce;

impl NonceStore for SkipNonce {
    fn check_and_insert<'a>(
        &'a self,
        _key: &'a spot_signed_command::KeyId,
        _nonce: [u8; 16],
        _expires_at: i64,
        _now: i64,
    ) -> NonceFuture<'a, Result<bool, StoreError>> {
        Box::pin(std::future::ready(Ok(true)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use serde_json::json;
    use spot_mgmt_ops::{
        Catalogue, CatalogueBuilder, OperationSpec, ParamSpec, ParamType, Risk, TargetKind,
    };

    use super::*;

    fn spec() -> OperationSpec {
        OperationSpec {
            id: OpId::new("server.service.restart").unwrap(),
            title: "Restart".to_owned(),
            summary: "Restarts a service.".to_owned(),
            params: vec![
                ParamSpec {
                    name: "name".to_owned(),
                    ty: ParamType::Text {
                        max_len: 32,
                        pattern: None,
                    },
                    required: true,
                    default: None,
                    doc: "The unit name.".to_owned(),
                    secret: false,
                },
                ParamSpec {
                    name: "wait".to_owned(),
                    ty: ParamType::Duration,
                    required: false,
                    default: Some(ParamValue::Duration(30)),
                    doc: "Seconds to wait.".to_owned(),
                    secret: false,
                },
            ],
            risk: Risk::Low,
            target: TargetKind::Server,
        }
    }

    fn catalogue() -> Catalogue {
        let mut builder = CatalogueBuilder::new();
        builder.add("test", spec());
        builder.build().unwrap()
    }

    #[test]
    fn converts_declared_parameters_and_fills_defaults() {
        let catalogue = catalogue();
        let spec = catalogue
            .get(&OpId::new("server.service.restart").unwrap())
            .unwrap();
        let invocation =
            invocation_from_json(spec, &json!({"name": "spot-runtime", "wait": 5})).unwrap();
        let validated = catalogue.validate(&invocation).unwrap();
        assert_eq!(validated.params.len(), 2);
    }

    #[test]
    fn a_wrong_type_and_an_unknown_parameter_are_refused() {
        let catalogue = catalogue();
        let spec = catalogue
            .get(&OpId::new("server.service.restart").unwrap())
            .unwrap();
        assert!(invocation_from_json(spec, &json!({"name": 3})).is_err());
        // The unknown parameter is caught by the catalogue, not by the conversion.
        let with_unknown = Invocation::new(OpId::new("server.service.restart").unwrap())
            .with("name", ParamValue::Text("x".to_owned()))
            .with("surprise", ParamValue::Int(1));
        assert!(catalogue.validate(&with_unknown).is_err());
    }

    #[test]
    fn a_non_object_params_is_refused() {
        let catalogue = catalogue();
        let spec = catalogue
            .get(&OpId::new("server.service.restart").unwrap())
            .unwrap();
        assert!(invocation_from_json(spec, &json!("nope")).is_err());
    }
}

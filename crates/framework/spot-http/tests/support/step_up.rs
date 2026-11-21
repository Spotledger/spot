//! A step-up port that always succeeds, for the suites that need the `auth_step_up` group mounted
//! (the conformance walk). The routes' own behaviour is tested with a scripted fake in
//! `tests/step_up_routes.rs`; who may step up and what a step-up records belong to the host's
//! adapter and are tested in `spot-cli`.

use std::sync::Arc;

use spot_http::api::ports::step_up::{StepUpOut, StepUpPort, StepUpProof, StepUpResult};
use spot_http::api::service::{BoxFuture, RequestCtx};

/// Answers a fixed instant and no redirect, whatever the proof.
pub struct AlwaysStepsUp;

impl StepUpPort for AlwaysStepsUp {
    fn step_up<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        _proof: StepUpProof,
    ) -> BoxFuture<'a, StepUpResult<StepUpOut>> {
        Box::pin(async {
            Ok(StepUpOut {
                step_up_until: Some("2026-11-21T09:05:00Z".to_owned()),
                redirect_url: None,
            })
        })
    }
}

/// The port as a suite provides it.
pub fn fake() -> Arc<dyn StepUpPort> {
    Arc::new(AlwaysStepsUp)
}

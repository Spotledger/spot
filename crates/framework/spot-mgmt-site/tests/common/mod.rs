//! Fakes for the handler tests: a scripted CLI, fixed facts and a control with or without a cancel
//! request.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use spot_mgmt_api::handlers::HandlerRequest;
use spot_mgmt_api::ports::BoxFuture;
use spot_mgmt_ops::{Catalogue, CatalogueBuilder, Invocation, OpId, OperationModule, ParamValue};
use spot_mgmt_site::{
    CliError, CliOutput, CliRequest, Control, Maintenance, SiteCli, SiteContext, SiteFacts,
    SiteOpsModule, Timeouts,
};
use spot_migration_ops::MigrationOpsModule;

pub const SITE: &str = "demo";

/// A CLI that answers from a script (the last answer repeats) and records every request.
pub struct FakeCli {
    pub calls: Mutex<Vec<CliRequest>>,
    script: Mutex<VecDeque<Result<CliOutput, CliError>>>,
    last: Mutex<Result<CliOutput, CliError>>,
    /// When set, `run` never completes (a long command, for the cancel test).
    pub hang: bool,
}

pub fn output(exit: u8, stdout: &str, stderr: &str) -> Result<CliOutput, CliError> {
    Ok(CliOutput {
        exit,
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    })
}

impl FakeCli {
    pub fn new(script: Vec<Result<CliOutput, CliError>>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            last: Mutex::new(output(0, "", "")),
            script: Mutex::new(script.into()),
            hang: false,
        })
    }

    pub fn hanging() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            last: Mutex::new(output(0, "", "")),
            script: Mutex::new(VecDeque::new()),
            hang: true,
        })
    }

    /// The recorded argument lists, comparable with `vec![vec!["a", "b"]]`.
    pub fn argv_list(&self) -> Vec<Vec<String>> {
        self.argvs()
    }

    pub fn argvs(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.argv.clone())
            .collect()
    }
}

impl SiteCli for FakeCli {
    fn run(&self, request: CliRequest) -> BoxFuture<'_, Result<CliOutput, CliError>> {
        self.calls.lock().unwrap().push(request);
        if self.hang {
            return Box::pin(std::future::pending());
        }
        let next = self.script.lock().unwrap().pop_front();
        let answer = match next {
            Some(answer) => {
                *self.last.lock().unwrap() = answer.clone();
                answer
            }
            None => self.last.lock().unwrap().clone(),
        };
        Box::pin(std::future::ready(answer))
    }
}

pub struct FakeFacts(pub Maintenance);

impl SiteFacts for FakeFacts {
    fn version(&self) -> String {
        "0.1.2".to_owned()
    }
    fn maintenance(&self) -> Maintenance {
        self.0.clone()
    }
}

/// A control with no cancel request.
pub struct Quiet {
    pub progress: Mutex<Vec<(String, u8)>>,
}

impl Quiet {
    pub fn new() -> Self {
        Self {
            progress: Mutex::new(Vec::new()),
        }
    }
}

impl Control for Quiet {
    fn progress<'a>(&'a self, step: &'a str, percent: u8) -> BoxFuture<'a, ()> {
        self.progress
            .lock()
            .unwrap()
            .push((step.to_owned(), percent));
        Box::pin(std::future::ready(()))
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

/// A control whose cancel request is already set.
pub struct CancelNow;

impl Control for CancelNow {
    fn progress<'a>(&'a self, _step: &'a str, _percent: u8) -> BoxFuture<'a, ()> {
        Box::pin(std::future::ready(()))
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::ready(()))
    }
}

pub fn context(cli: Arc<FakeCli>) -> SiteContext {
    SiteContext {
        site: SITE.to_owned(),
        cli,
        facts: Arc::new(FakeFacts(Maintenance::Open)),
        migrations: None,
        timeouts: Timeouts::default(),
    }
}

pub fn catalogue() -> Catalogue {
    let mut builder = CatalogueBuilder::new();
    SiteOpsModule.register(&mut builder);
    MigrationOpsModule.register(&mut builder);
    builder.build().unwrap()
}

/// A request that passed the catalogue's validation, as the server would hand it over.
pub fn request(operation: &str, params: &[(&str, &str)]) -> HandlerRequest {
    let id = OpId::new(operation).unwrap();
    let mut invocation = Invocation::new(id.clone());
    for (name, value) in params {
        invocation = invocation.with(*name, ParamValue::Text((*value).to_owned()));
    }
    let params = catalogue().validate(&invocation).unwrap();
    HandlerRequest {
        command_id: "c1".to_owned(),
        principal: "alice".to_owned(),
        operation: id,
        params,
    }
}

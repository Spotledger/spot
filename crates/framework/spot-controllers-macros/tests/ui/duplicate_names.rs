#![allow(unused_imports, dead_code)]
use serde::{Deserialize, Serialize};
use spot_app_api::{AppResult, MethodContext};
use spot_controllers_macros::{controller, whitelist, MethodSchema};

#[derive(Deserialize, MethodSchema)]
pub struct In {
    pub a: String,
}

#[derive(Serialize, MethodSchema)]
pub struct Out {
    pub b: String,
}

pub struct C;

#[controller(doctype = "note")]
impl C {
    #[whitelist]
    async fn first(&self, _ctx: &mut dyn MethodContext) -> AppResult<()> { Ok(()) }
    #[whitelist(name = "first")]
    async fn second(&self, _ctx: &mut dyn MethodContext) -> AppResult<()> { Ok(()) }
}
fn main() {}

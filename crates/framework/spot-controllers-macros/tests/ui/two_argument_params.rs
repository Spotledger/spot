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
    async fn go(&self, _ctx: &mut dyn MethodContext, _a: In, _b: In) -> AppResult<()> { Ok(()) }
}
fn main() {}

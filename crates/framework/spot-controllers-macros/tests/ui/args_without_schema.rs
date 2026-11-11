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

#[derive(Deserialize)]
pub struct Plain {
    pub a: String,
}

#[controller(doctype = "note")]
impl C {
    #[whitelist]
    async fn go(&self, _ctx: &mut dyn MethodContext, _args: Plain) -> AppResult<()> { Ok(()) }
}
fn main() {}

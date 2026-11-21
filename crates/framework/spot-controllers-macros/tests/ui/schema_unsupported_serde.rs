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

#[derive(Deserialize, MethodSchema)]
#[serde(rename_all = "camelCase")]
pub struct Camel {
    pub some_field: String,
}
fn main() {}

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

pub trait T { fn t(&self); }

#[controller(doctype = "note")]
impl T for C {
    fn t(&self) {}
}
fn main() {}

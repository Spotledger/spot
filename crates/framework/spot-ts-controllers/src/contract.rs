//! The controller module contract: what a TypeScript module must declare, and how its declaration
//! becomes the same [`StepDecl`]s and [`MethodDecl`]s a native controller declares.
//!
//! A module exports `contract()`, a function returning the declaration as JSON:
//!
//! ```json
//! {
//!   "doctype": "invoice",
//!   "steps": {
//!     "derive_total": { "point": "before_save", "priority": 10 },
//!     "check_credit": { "point": "validate" }
//!   },
//!   "methods": {
//!     "approve": {
//!       "right": "write",
//!       "args": { "amount": "decimal", "note": "text?" },
//!       "result": { "ok": "bool" }
//!     }
//!   }
//! }
//! ```
//!
//! [`parse_contract`] validates it and reports **every** problem at once, each with the position of
//! the declaration that is wrong when the engine knows one.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json, json};
use spot_app_api::lifecycle::{HookPoint, Operation, Phase, Target};
use spot_app_api::prelude::Name;
use spot_app_api::{MethodDecl, MethodRight, StepDecl, StepName};
use std::time::Duration;

use crate::codes;
use crate::problem::Problem;
use crate::schema::{ArgsSchema, result_schema_from_json};

/// The step priorities the platform reserves; a module's own priority is added to the point's
/// base so points that share a phase keep their ADR-51 order.
const POINT_BASE: i32 = 10_000;

/// One declared lifecycle step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepSpec {
    /// The step's name within the module (a storage name); the registered step is `<app>:<name>`.
    pub name: String,
    /// Where it runs.
    pub point: HookPoint,
    /// Lower runs first among the steps of one point.
    pub priority: i32,
    /// The handler's wall-clock timeout, in milliseconds (0 means the site default).
    pub timeout_ms: u64,
}

/// One declared whitelisted method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodSpec {
    /// The method's name (a storage name).
    pub name: String,
    /// The right it needs: one of `read`, `write`, `submit`, `cancel`, `create`, or an
    /// app-registered permission.
    pub right: String,
    /// Whether it changes documents; the native API then requires `If-Match`.
    pub mutates: bool,
    /// Its argument declaration.
    pub args: ArgsSchema,
    /// Its result schema.
    pub result: Json,
}

/// A validated module declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contract {
    /// The DocType the module is the controller of.
    pub doctype: String,
    /// Its steps, in name order.
    pub steps: Vec<StepSpec>,
    /// Its methods, in name order.
    pub methods: BTreeMap<String, MethodSpec>,
}

impl Contract {
    /// The steps as [`StepDecl`]s registered under `app` (`<app>:<name>`).
    ///
    /// # Errors
    ///
    /// A `platform.invalid_step_name` error when `app` or a step name is not a storage name; the
    /// module was validated at load, so this only fires for a host that passes a bad app name.
    pub fn step_decls(&self, app: &str) -> Result<Vec<StepDecl>, spot_app_api::AppError> {
        self.steps
            .iter()
            .map(|step| {
                Ok(StepDecl {
                    name: StepName::new(&format!("{app}:{}", step.name))?,
                    point: step.point.clone(),
                    priority: step.priority,
                    budget: crate::limits::TsLimits::default().timeout(step.timeout_ms)
                        + Duration::from_millis(50),
                })
            })
            .collect()
    }

    /// The methods as [`MethodDecl`]s, for `Controller::methods`.
    pub fn method_decls(&self) -> Vec<MethodDecl> {
        self.methods
            .values()
            .filter_map(|method| {
                // A method whose name or right does not validate is left out (it cannot be
                // called) rather than declared with a right it did not ask for.
                let name = Name::new(&method.name).ok()?;
                let right = method_right(&method.right)?;
                Some(MethodDecl::new(name, method.mutates).with_right(right))
            })
            .collect()
    }

    /// Each method's right, `mutates` flag and JSON Schemas, keyed by name: the same shape
    /// `#[whitelist]`'s `whitelisted_schemas()` serves, so the API layer needs no special case.
    pub fn schemas(&self) -> Json {
        let mut out = Map::new();
        for (name, method) in &self.methods {
            let custom = !matches!(
                method.right.as_str(),
                "read" | "write" | "submit" | "cancel" | "create"
            );
            out.insert(
                name.clone(),
                json!({
                    "right": method.right,
                    "custom_permission": custom,
                    "mutates": method.mutates,
                    "arguments": method.args.schema(),
                    "result": method.result,
                }),
            );
        }
        Json::Object(out)
    }
}

/// Parses and validates a module's `contract()` result.
///
/// # Errors
///
/// Every problem at once; each carries the dotted path of the declaration and, when known, the
/// source position.
pub fn parse_contract(value: &Json) -> Result<Contract, Vec<Problem>> {
    let mut problems = Vec::new();
    let Some(object) = value.as_object() else {
        return Err(vec![Problem::new(
            codes::INVALID_MODULE,
            "contract",
            "`contract()` must return an object",
        )]);
    };
    let doctype = match object.get("doctype").and_then(Json::as_str) {
        Some(text) if Name::new(text).is_ok() => text.to_owned(),
        Some(text) => {
            problems.push(Problem::new(
                codes::INVALID_MODULE,
                "contract/doctype",
                format!("`{text}` is not a DocType storage name"),
            ));
            String::new()
        }
        None => {
            problems.push(Problem::new(
                codes::INVALID_MODULE,
                "contract/doctype",
                "a controller module declares its DocType",
            ));
            String::new()
        }
    };

    let mut steps = Vec::new();
    match object.get("steps") {
        None => {}
        Some(Json::Object(map)) => {
            for (name, spec) in map {
                match parse_step(name, spec) {
                    Ok(step) => steps.push(step),
                    Err(mut found) => problems.append(&mut found),
                }
            }
        }
        Some(_) => problems.push(Problem::new(
            codes::INVALID_MODULE,
            "contract/steps",
            "`steps` must be an object keyed by step name",
        )),
    }
    steps.sort_by(|a, b| a.name.cmp(&b.name));

    let mut methods = BTreeMap::new();
    match object.get("methods") {
        None => {}
        Some(Json::Object(map)) => {
            for (name, spec) in map {
                match parse_method(name, spec) {
                    Ok(method) => {
                        methods.insert(name.clone(), method);
                    }
                    Err(mut found) => problems.append(&mut found),
                }
            }
        }
        Some(_) => problems.push(Problem::new(
            codes::INVALID_MODULE,
            "contract/methods",
            "`methods` must be an object keyed by method name",
        )),
    }

    if problems.is_empty() {
        Ok(Contract {
            doctype,
            steps,
            methods,
        })
    } else {
        Err(problems)
    }
}

fn parse_step(name: &str, spec: &Json) -> Result<StepSpec, Vec<Problem>> {
    let path = format!("contract/steps/{name}");
    let mut problems = Vec::new();
    if Name::new(name).is_err() {
        problems.push(Problem::new(
            codes::INVALID_MODULE,
            &path,
            format!("`{name}` is not a storage-style step name"),
        ));
    }
    let Some(object) = spec.as_object() else {
        problems.push(Problem::new(
            codes::INVALID_MODULE,
            &path,
            "a step is an object `{ point, priority? }`",
        ));
        return Err(problems);
    };
    let point = match object.get("point").and_then(Json::as_str) {
        Some(text) => match hook_point(text) {
            Some(point) => Some(point),
            None => {
                problems.push(Problem::new(
                    codes::INVALID_MODULE,
                    &format!("{path}.point"),
                    format!("`{text}` is not a lifecycle hook point"),
                ));
                None
            }
        },
        None => {
            problems.push(Problem::new(
                codes::INVALID_MODULE,
                &format!("{path}.point"),
                "a step declares its hook point",
            ));
            None
        }
    };
    let priority = match object.get("priority") {
        None => 0,
        Some(value) => match value.as_i64().and_then(|n| i32::try_from(n).ok()) {
            Some(priority) => priority,
            None => {
                problems.push(Problem::new(
                    codes::INVALID_MODULE,
                    &format!("{path}.priority"),
                    "`priority` is an integer",
                ));
                0
            }
        },
    };
    let timeout_ms = match object.get("timeout_ms") {
        None => 0,
        Some(value) => match value.as_u64() {
            Some(ms) => ms,
            None => {
                problems.push(Problem::new(
                    codes::INVALID_MODULE,
                    &format!("{path}.timeout_ms"),
                    "`timeout_ms` is a non-negative integer",
                ));
                0
            }
        },
    };
    match point {
        Some(point) if problems.is_empty() => Ok(StepSpec {
            name: name.to_owned(),
            point,
            priority,
            timeout_ms,
        }),
        _ => Err(problems),
    }
}

/// A module's declared `right` as the contract's [`MethodRight`]: the five operations by name,
/// anything else an app permission (already validated as a storage name by [`parse_contract`]).
/// `None` when a permission name does not validate.
fn method_right(right: &str) -> Option<MethodRight> {
    Some(match right {
        "read" => MethodRight::Read,
        "write" => MethodRight::Write,
        "create" => MethodRight::Create,
        "submit" => MethodRight::Submit,
        "cancel" => MethodRight::Cancel,
        other => MethodRight::Permission(Name::new(other).ok()?),
    })
}

fn parse_method(name: &str, spec: &Json) -> Result<MethodSpec, Vec<Problem>> {
    let path = format!("contract/methods/{name}");
    let mut problems = Vec::new();
    if Name::new(name).is_err() {
        problems.push(Problem::new(
            codes::INVALID_MODULE,
            &path,
            format!("`{name}` is not a storage-style method name"),
        ));
    }
    let Some(object) = spec.as_object() else {
        problems.push(Problem::new(
            codes::INVALID_MODULE,
            &path,
            "a method is an object `{ right, args?, result?, mutates? }`",
        ));
        return Err(problems);
    };
    let right = match object.get("right").and_then(Json::as_str) {
        Some(text) if Name::new(text).is_ok() => text.to_owned(),
        Some(text) => {
            problems.push(Problem::new(
                codes::INVALID_MODULE,
                &format!("{path}.right"),
                format!("`{text}` is not a right or a permission name"),
            ));
            String::new()
        }
        None => {
            problems.push(Problem::new(
                codes::INVALID_MODULE,
                &format!("{path}.right"),
                "a method declares the right it needs (`read`, `write`, or a permission)",
            ));
            String::new()
        }
    };
    let mut mutates = match object.get("mutates") {
        None => right != "read",
        Some(value) => match value.as_bool() {
            Some(flag) => flag,
            None => {
                problems.push(Problem::new(
                    codes::INVALID_MODULE,
                    &format!("{path}.mutates"),
                    "`mutates` is true or false",
                ));
                right != "read"
            }
        },
    };
    let args = match object.get("args") {
        None => ArgsSchema::empty(),
        Some(value) => match ArgsSchema::from_json(value, &format!("{path}.args")) {
            Ok(args) => args,
            Err(mut found) => {
                problems.append(&mut found);
                ArgsSchema::empty()
            }
        },
    };
    let result = match result_schema_from_json(object.get("result"), &format!("{path}.result")) {
        Ok(schema) => schema,
        Err(mut found) => {
            problems.append(&mut found);
            json!({})
        }
    };
    if right == "read" && mutates {
        mutates = false;
    }
    if problems.is_empty() {
        Ok(MethodSpec {
            name: name.to_owned(),
            right,
            mutates,
            args,
            result,
        })
    } else {
        Err(problems)
    }
}

/// The hook point a declaration names, or `None`.
///
/// Accepts the ADR-51 names (`before_save`, `validate_save`, `after_post`, `save_committed`), the
/// Frappe-familiar names (`validate`, `before_submit`, `on_submit`, `before_cancel`, `on_cancel`,
/// `after_commit`), `load` and `field_changed:<field>`.
pub fn hook_point(text: &str) -> Option<HookPoint> {
    match text {
        "load" => return Some(HookPoint::Load),
        "before_validate" => return Some(HookPoint::of(Phase::Before, Target::Save)),
        "validate" => return Some(HookPoint::of(Phase::Validate, Target::Save)),
        "before_submit" => return Some(HookPoint::of(Phase::Before, Target::Op(Operation::Post))),
        "on_submit" => return Some(HookPoint::of(Phase::After, Target::Op(Operation::Post))),
        "before_cancel" => {
            return Some(HookPoint::of(
                Phase::Validate,
                Target::Op(Operation::Cancel),
            ));
        }
        "on_cancel" => return Some(HookPoint::of(Phase::After, Target::Op(Operation::Cancel))),
        "after_commit" => return Some(HookPoint::of(Phase::Committed, Target::Save)),
        _ => {}
    }
    if let Some(field) = text.strip_prefix("field_changed:") {
        return Name::new(field)
            .ok()
            .map(|name| HookPoint::FieldChanged(name.as_str().to_owned()));
    }
    let (phase, rest) = if let Some(rest) = text.strip_prefix("before_") {
        (Phase::Before, rest)
    } else if let Some(rest) = text.strip_prefix("validate_") {
        (Phase::Validate, rest)
    } else if let Some(rest) = text.strip_prefix("after_") {
        (Phase::After, rest)
    } else {
        (Phase::Committed, text.strip_suffix("_committed")?)
    };
    let target = match rest {
        "save" => Target::Save,
        "create" => Target::Op(Operation::Create),
        "update" => Target::Op(Operation::Update),
        "update_posted" => Target::Op(Operation::UpdatePosted),
        "post" => Target::Op(Operation::Post),
        "cancel" => Target::Op(Operation::Cancel),
        "delete" => Target::Op(Operation::Delete),
        "rename" => Target::Op(Operation::Rename),
        _ => return None,
    };
    Some(HookPoint::of(phase, target))
}

/// The base priority of a hook point, so points sharing a phase keep their ADR-51 order whatever
/// order a module declares them in.
pub fn point_base_priority(point: &HookPoint) -> i32 {
    let rank = match point.name().as_str() {
        "before_validate" => 0,
        "validate_save" => 1,
        "before_save" => 2,
        "after_save" => 3,
        "before_post" => 4,
        "after_post" => 5,
        "validate_cancel" => 6,
        "after_cancel" => 7,
        "save_committed" => 8,
        _ => 9,
    };
    rank * POINT_BASE
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn contract() -> Contract {
        parse_contract(&json!({
            "doctype": "invoice",
            "steps": { "derive_total": { "point": "before_save", "priority": 10 } },
            "methods": {
                "approve": { "right": "write", "args": { "amount": "decimal" }, "result": { "ok": "bool" } },
                "peek": { "right": "read", "mutates": true }
            }
        }))
        .unwrap()
    }

    #[test]
    fn a_contract_becomes_step_and_method_decls() {
        let contract = contract();
        let steps = contract.step_decls("erp_selling").unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name.as_str(), "erp_selling:derive_total");
        assert_eq!(steps[0].point, hook_point("before_save").unwrap());
        let methods = contract.method_decls();
        assert_eq!(methods.len(), 2);
        let peek = methods.iter().find(|m| m.name.as_str() == "peek").unwrap();
        assert!(!peek.mutates, "a read right forces mutates false");
    }

    #[test]
    fn a_declared_right_reaches_the_method_decl() {
        let contract = parse_contract(&json!({
            "doctype": "invoice",
            "methods": {
                "approve": { "right": "write" },
                "peek": { "right": "read" },
                "post_it": { "right": "submit" },
                "void_it": { "right": "cancel" },
                "make": { "right": "create" },
                "audit": { "right": "invoice_audit" },
                "audit_report": { "right": "invoice_audit", "mutates": false }
            }
        }))
        .unwrap();
        let methods = contract.method_decls();
        let right = |name: &str| {
            methods
                .iter()
                .find(|m| m.name.as_str() == name)
                .unwrap()
                .right
                .clone()
        };
        assert_eq!(right("approve"), MethodRight::Write);
        assert_eq!(right("peek"), MethodRight::Read);
        assert_eq!(right("post_it"), MethodRight::Submit);
        assert_eq!(right("void_it"), MethodRight::Cancel);
        assert_eq!(right("make"), MethodRight::Create);
        let permission = MethodRight::Permission(Name::new("invoice_audit").unwrap());
        assert_eq!(right("audit"), permission);
        assert_eq!(right("audit_report"), permission);
        assert!(methods.iter().all(|m| !m.contradicts()));
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let problems = parse_contract(&json!({
            "doctype": "Not A Name",
            "steps": { "bad step": { "point": "nope" } },
            "methods": { "m": { "right": "not a name" } }
        }))
        .unwrap_err();
        assert!(problems.len() >= 3, "got {problems:?}");
    }

    #[test]
    fn hook_points_parse_both_name_families() {
        assert_eq!(
            hook_point("before_submit"),
            Some(HookPoint::of(Phase::Before, Target::Op(Operation::Post)))
        );
        assert_eq!(
            hook_point("validate_cancel").map(|p| p.name()),
            Some("validate_cancel".into())
        );
        assert_eq!(hook_point("load").map(|p| p.name()), Some("load".into()));
        assert!(hook_point("whenever").is_none());
    }
}

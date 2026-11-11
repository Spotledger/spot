//! Invocation validation: [`Catalogue::validate`] checks an [`Invocation`] against its
//! operation's spec and fills the defaults into a [`Validated`]. Every problem is
//! reported together, in a deterministic order.

use crate::error::InvocationError;
use crate::validated::{AcceptedParam, Validated};
use crate::value_check::check_value;

/// Validates `invocation` against the catalogue: resolves the operation (unknown
/// operation is the first problem and short-circuits everything else, since there is
/// nothing to validate against), then reports — all together — unknown parameters, missing
/// required parameters, wrong types, out-of-range values, pattern mismatches, enum
/// membership and length violations. Defaults are filled into the returned [`Validated`]
/// when the invocation is otherwise acceptable.
pub(crate) fn validate(
    catalogue: &crate::Catalogue,
    invocation: &crate::Invocation,
) -> Result<Validated, Vec<InvocationError>> {
    let op = invocation.op.clone();
    let Some(spec) = catalogue.get(&op) else {
        return Err(vec![InvocationError::UnknownOp {
            op: op.as_str().to_owned(),
        }]);
    };

    let mut problems: Vec<InvocationError> = Vec::new();

    // Unknown parameters first: whatever the caller supplied that no declaration names,
    // in the invocation's (sorted) key order.
    for name in invocation.params.keys() {
        if !spec.params.iter().any(|p| p.name == *name) {
            problems.push(InvocationError::UnknownParam {
                op: op.as_str().to_owned(),
                param: name.clone(),
            });
        }
    }

    // Then every declared parameter, in declaration order: supplied values are checked,
    // omitted required ones are reported, omitted optional ones take their default.
    let mut accepted: Vec<AcceptedParam> = Vec::new();
    for param in &spec.params {
        match invocation.params.get(&param.name) {
            Some(value) => {
                check_value(op.as_str(), &param.name, &param.ty, value, &mut problems);
                accepted.push(AcceptedParam {
                    ty: param.ty.clone(),
                    value: value.clone(),
                    from_default: false,
                    secret: param.secret,
                    name: param.name.clone(),
                });
            }
            None => {
                if param.required {
                    problems.push(InvocationError::MissingParam {
                        op: op.as_str().to_owned(),
                        param: param.name.clone(),
                    });
                } else if let Some(default) = &param.default {
                    accepted.push(AcceptedParam {
                        ty: param.ty.clone(),
                        value: default.clone(),
                        from_default: true,
                        secret: param.secret,
                        name: param.name.clone(),
                    });
                }
            }
        }
    }

    if problems.is_empty() {
        return Ok(Validated {
            op,
            params: accepted,
        });
    }
    Err(problems)
}

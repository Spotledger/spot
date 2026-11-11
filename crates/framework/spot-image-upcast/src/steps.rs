//! The declarative step builders a patch is written with.
//!
//! A step is a pure function of the image: it rewrites the body of an image that sits at the
//! step's `from` version into the shape of its `to` version, and returns
//! [`UpcastError::StepShape`] when the body is a shape it cannot rewrite without losing a stored
//! value. No builder reads a clock, a random source, a file or a key.
//!
//! # Rules every builder keeps
//!
//! * **An absent source is not an error.** Images differ in which optional fields they carry, so
//!   a step whose source field is absent does nothing. This is what makes re-applying a chain a
//!   no-op rather than a second rewrite.
//! * **A step never overwrites a stored value.** Where the target already holds something
//!   (`rename_field`'s new name, `move_field_to_child`'s row field) the step refuses with
//!   [`UpcastError::StepShape`] instead of dropping data.
//! * **A refused step leaves the image untouched**, so the caller may retry or report.

use serde_json::Value;

use crate::error::UpcastError;
use crate::image::Image;

/// One registered rewrite of an image's body.
///
/// Boxed rather than a bare function pointer so a builder can carry its field names and default
/// values without every patch declaring a `fn`.
pub type Step = Box<dyn Fn(&mut Image) -> Result<(), UpcastError>>;

/// The source field name, kept alive by the closure.
fn name(field: &str) -> String {
    field.to_owned()
}

/// Renames a field, keeping its value.
///
/// The value stored under `from` moves to `to`; an image without `from` is unchanged. A
/// `from == to` rename is a no-op, and a body that already holds `to` is refused rather than
/// overwritten.
#[must_use]
pub fn rename_field(from: &str, to: &str) -> Step {
    let from = name(from);
    let to = name(to);
    Box::new(move |image: &mut Image| {
        if from == to {
            return Ok(());
        }
        let body = image.body_mut();
        let Some(value) = body.remove(&from) else {
            return Ok(());
        };
        if body.contains_key(&to) {
            body.insert(from.clone(), value);
            return Err(UpcastError::StepShape {
                step: "rename_field",
                field: to.clone(),
                reason: "already exists, so the rename would overwrite it",
            });
        }
        body.insert(to.clone(), value);
        Ok(())
    })
}

/// Drops a field that the new schema no longer declares.
///
/// An image without the field is unchanged: dropping is idempotent by construction.
#[must_use]
pub fn drop_field(field: &str) -> Step {
    let field = name(field);
    Box::new(move |image: &mut Image| {
        image.body_mut().remove(&field);
        Ok(())
    })
}

/// Adds a field the new schema declares, with the value an image written before it must read.
///
/// The default is inserted only when the field is absent: an image that already carries a value
/// keeps it, so re-applying the step cannot reset a stored value.
#[must_use]
pub fn add_field_with_default(field: &str, default: Value) -> Step {
    let field = name(field);
    Box::new(move |image: &mut Image| {
        let body = image.body_mut();
        if !body.contains_key(&field) {
            body.insert(field.clone(), default.clone());
        }
        Ok(())
    })
}

/// Moves a top-level field into every row of a child table.
///
/// The value of `parent_field` is written to `row_field` in **every** row of the child table
/// `child`, and the top-level field is removed: a header value the new schema declares per row is
/// denormalised into the rows. Rows are addressed positionally, in stored order.
///
/// It is refused (leaving the image untouched) when the child table is missing or is not an
/// array, when a row is not an object, or when a row already holds `row_field`. When the child
/// table is an array with no rows there is nothing to carry the value, so the parent field is
/// left in place rather than dropped.
#[must_use]
pub fn move_field_to_child(child: &str, parent_field: &str, row_field: &str) -> Step {
    let child = name(child);
    let parent = name(parent_field);
    let row = name(row_field);
    Box::new(move |image: &mut Image| {
        let Some(value) = image.field(&parent).cloned() else {
            return Ok(());
        };
        let Some(rows) = image.rows(&child) else {
            return Err(UpcastError::StepShape {
                step: "move_field_to_child",
                field: child.clone(),
                reason: "is not a child table (an array of rows)",
            });
        };
        for stored in rows {
            match stored {
                Value::Object(fields) if !fields.contains_key(&row) => {}
                Value::Object(_) => {
                    return Err(UpcastError::StepShape {
                        step: "move_field_to_child",
                        field: row.clone(),
                        reason: "already exists in a child row, so the move would overwrite it",
                    });
                }
                _ => {
                    return Err(UpcastError::StepShape {
                        step: "move_field_to_child",
                        field: child.clone(),
                        reason: "holds a row that is not an object",
                    });
                }
            }
        }
        if rows.is_empty() {
            return Ok(());
        }
        let body = image.body_mut();
        // Every row was checked above, so this match cannot skip a row.
        if let Some(Value::Array(rows)) = body.get_mut(&child) {
            for stored in rows.iter_mut() {
                if let Value::Object(fields) = stored {
                    fields.insert(row.clone(), value.clone());
                }
            }
        }
        body.remove(&parent);
        Ok(())
    })
}

/// Retypes a field through a converter, keeping the stored value's meaning.
///
/// `convert` receives the stored value and returns the value the new type encodes, or a reason
/// (a phrase naming the shape, never the value). A converter is how a patch turns a decimal
/// string into the new encoding, an integer into a decimal string, or a flag into an enum name:
/// the crate never does arithmetic of its own, and never on a float.
///
/// An image without the field is unchanged; a value the converter refuses is
/// [`UpcastError::ConvertFailed`] naming the field and the reason.
#[must_use]
pub fn retype_field<F>(field: &str, convert: F) -> Step
where
    F: Fn(&Value) -> Result<Value, String> + 'static,
{
    let field = name(field);
    Box::new(move |image: &mut Image| {
        let Some(stored) = image.field(&field).cloned() else {
            return Ok(());
        };
        match convert(&stored) {
            Ok(converted) => {
                image.body_mut().insert(field.clone(), converted);
                Ok(())
            }
            Err(reason) => Err(UpcastError::ConvertFailed {
                field: field.clone(),
                reason,
            }),
        }
    })
}

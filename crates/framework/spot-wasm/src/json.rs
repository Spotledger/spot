//! JSON to and from component values, driven by the export's declared types.
//!
//! | WIT type | JSON |
//! | --- | --- |
//! | `bool` | `true` / `false` |
//! | `s8`..`s64`, `u8`..`u64` | an integer in range (never a fraction) |
//! | `char` | a string of one character |
//! | `string` | a string |
//! | `list<T>`, `tuple<..>` | an array |
//! | `record` | an object with exactly the record's fields |
//! | `variant` | `{"case": "name"}` or `{"case": "name", "value": payload}` |
//! | `enum`, `flags` | a string, an array of strings |
//! | `option<T>` | `null` for none, otherwise the value (so `option<option<T>>` is not supported) |
//! | `result<T, E>` | `{"ok": value}` or `{"err": value}` (a missing payload is `null`) |
//!
//! Floating point types, resources, futures and streams are not supported: no operation of this
//! host uses binary floating point.

use serde_json::{Map, Number, Value};
use wasmtime::component::{Type, Val};

use crate::error::{Result, WasmError};

/// Builds the `Val` for one side of a `result` from its payload.
type Wrap = fn(Option<Box<Val>>) -> Val;

fn bad(path: &str, why: impl std::fmt::Display) -> WasmError {
    WasmError::BadArguments(format!("{path}: {why}"))
}

/// Converts `json` to a value of type `ty`. `path` names the argument in errors.
pub(crate) fn to_val(json: &Value, ty: &Type, path: &str) -> Result<Val> {
    let integer = |min: i128, max: i128| -> Result<i128> {
        let n = match json {
            Value::Number(n) => n
                .as_i64()
                .map(i128::from)
                .or_else(|| n.as_u64().map(i128::from)),
            _ => None,
        };
        n.filter(|n| (min..=max).contains(n))
            .ok_or_else(|| bad(path, format!("expected an integer from {min} to {max}")))
    };
    // The range checks make each narrowing exact.
    Ok(match ty {
        Type::Bool => Val::Bool(
            json.as_bool()
                .ok_or_else(|| bad(path, "expected true or false"))?,
        ),
        Type::S8 => Val::S8(
            i8::try_from(integer(i8::MIN.into(), i8::MAX.into())?).map_err(|e| bad(path, e))?,
        ),
        Type::U8 => Val::U8(u8::try_from(integer(0, u8::MAX.into())?).map_err(|e| bad(path, e))?),
        Type::S16 => Val::S16(
            i16::try_from(integer(i16::MIN.into(), i16::MAX.into())?).map_err(|e| bad(path, e))?,
        ),
        Type::U16 => {
            Val::U16(u16::try_from(integer(0, u16::MAX.into())?).map_err(|e| bad(path, e))?)
        }
        Type::S32 => Val::S32(
            i32::try_from(integer(i32::MIN.into(), i32::MAX.into())?).map_err(|e| bad(path, e))?,
        ),
        Type::U32 => {
            Val::U32(u32::try_from(integer(0, u32::MAX.into())?).map_err(|e| bad(path, e))?)
        }
        Type::S64 => Val::S64(
            i64::try_from(integer(i64::MIN.into(), i64::MAX.into())?).map_err(|e| bad(path, e))?,
        ),
        Type::U64 => {
            Val::U64(u64::try_from(integer(0, u64::MAX.into())?).map_err(|e| bad(path, e))?)
        }
        Type::Char => {
            let text = json
                .as_str()
                .ok_or_else(|| bad(path, "expected a one-character string"))?;
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Val::Char(c),
                _ => return Err(bad(path, "expected a one-character string")),
            }
        }
        Type::String => Val::String(
            json.as_str()
                .ok_or_else(|| bad(path, "expected a string"))?
                .to_owned(),
        ),
        Type::List(list) => {
            let items = json
                .as_array()
                .ok_or_else(|| bad(path, "expected an array"))?;
            let element = list.ty();
            Val::List(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| to_val(v, &element, &format!("{path}[{i}]")))
                    .collect::<Result<_>>()?,
            )
        }
        Type::Tuple(tuple) => {
            let items = json
                .as_array()
                .ok_or_else(|| bad(path, "expected an array"))?;
            let types: Vec<Type> = tuple.types().collect();
            if items.len() != types.len() {
                return Err(bad(
                    path,
                    format!("expected {} elements, found {}", types.len(), items.len()),
                ));
            }
            Val::Tuple(
                items
                    .iter()
                    .zip(&types)
                    .enumerate()
                    .map(|(i, (v, t))| to_val(v, t, &format!("{path}[{i}]")))
                    .collect::<Result<_>>()?,
            )
        }
        Type::Record(record) => {
            let object = json
                .as_object()
                .ok_or_else(|| bad(path, "expected an object"))?;
            let fields: Vec<_> = record.fields().collect();
            for key in object.keys() {
                if !fields.iter().any(|f| f.name == key) {
                    return Err(bad(path, format!("unknown field `{key}`")));
                }
            }
            let mut out = Vec::with_capacity(fields.len());
            for field in fields {
                let value = object
                    .get(field.name)
                    .ok_or_else(|| bad(path, format!("missing field `{}`", field.name)))?;
                out.push((
                    field.name.to_owned(),
                    to_val(value, &field.ty, &format!("{path}.{}", field.name))?,
                ));
            }
            Val::Record(out)
        }
        Type::Variant(variant) => {
            let object = json
                .as_object()
                .ok_or_else(|| bad(path, "expected {\"case\": ...}"))?;
            let name = object
                .get("case")
                .and_then(Value::as_str)
                .ok_or_else(|| bad(path, "a variant needs a `case` string"))?;
            let case = variant
                .cases()
                .find(|c| c.name == name)
                .ok_or_else(|| bad(path, format!("unknown case `{name}`")))?;
            let payload = match (&case.ty, object.get("value")) {
                (Some(ty), Some(v)) => Some(Box::new(to_val(v, ty, &format!("{path}.value"))?)),
                (None, None | Some(Value::Null)) => None,
                (Some(_), None) => return Err(bad(path, format!("case `{name}` needs a `value`"))),
                (None, Some(_)) => return Err(bad(path, format!("case `{name}` has no payload"))),
            };
            Val::Variant(name.to_owned(), payload)
        }
        Type::Enum(e) => {
            let name = json
                .as_str()
                .ok_or_else(|| bad(path, "expected a string"))?;
            if !e.names().any(|n| n == name) {
                return Err(bad(
                    path,
                    format!(
                        "`{name}` is not one of {}",
                        e.names().collect::<Vec<_>>().join(", ")
                    ),
                ));
            }
            Val::Enum(name.to_owned())
        }
        Type::Flags(flags) => {
            let items = json
                .as_array()
                .ok_or_else(|| bad(path, "expected an array of flag names"))?;
            let mut out = Vec::new();
            for item in items {
                let name = item
                    .as_str()
                    .ok_or_else(|| bad(path, "flag names are strings"))?;
                if !flags.names().any(|n| n == name) {
                    return Err(bad(path, format!("unknown flag `{name}`")));
                }
                out.push(name.to_owned());
            }
            Val::Flags(out)
        }
        Type::Option(option) => match json {
            Value::Null => Val::Option(None),
            other => Val::Option(Some(Box::new(to_val(other, &option.ty(), path)?))),
        },
        Type::Result(result) => {
            let object = json
                .as_object()
                .ok_or_else(|| bad(path, "expected {\"ok\": ...} or {\"err\": ...}"))?;
            let (side, ty, wrap): (&str, Option<Type>, Wrap) =
                match (object.get("ok"), object.get("err")) {
                    (Some(_), None) => ("ok", result.ok(), |v| Val::Result(Ok(v))),
                    (None, Some(_)) => ("err", result.err(), |v| Val::Result(Err(v))),
                    _ => return Err(bad(path, "a result is exactly one of `ok` or `err`")),
                };
            let value = object.get(side).unwrap_or(&Value::Null);
            let payload = match ty {
                Some(ty) => Some(Box::new(to_val(value, &ty, &format!("{path}.{side}"))?)),
                None => None,
            };
            wrap(payload)
        }
        other => return Err(bad(path, format!("the type {other:?} is not supported"))),
    })
}

/// Converts a component value to JSON; see the module documentation for the shapes.
pub(crate) fn to_json(val: &Val) -> Result<Value> {
    let unsupported = |what: &str| {
        WasmError::Internal(format!(
            "a result of type {what} cannot be converted to JSON"
        ))
    };
    let number = |n: i128| -> Result<Value> {
        i64::try_from(n)
            .map(|n| Value::Number(Number::from(n)))
            .or_else(|_| u64::try_from(n).map(|n| Value::Number(Number::from(n))))
            .map_err(|_| unsupported("integer"))
    };
    Ok(match val {
        Val::Bool(b) => Value::Bool(*b),
        Val::S8(n) => number(i128::from(*n))?,
        Val::U8(n) => number(i128::from(*n))?,
        Val::S16(n) => number(i128::from(*n))?,
        Val::U16(n) => number(i128::from(*n))?,
        Val::S32(n) => number(i128::from(*n))?,
        Val::U32(n) => number(i128::from(*n))?,
        Val::S64(n) => number(i128::from(*n))?,
        Val::U64(n) => number(i128::from(*n))?,
        Val::Char(c) => Value::String(c.to_string()),
        Val::String(s) => Value::String(s.clone()),
        Val::List(items) | Val::Tuple(items) => {
            Value::Array(items.iter().map(to_json).collect::<Result<_>>()?)
        }
        Val::Record(fields) => {
            let mut object = Map::new();
            for (name, value) in fields {
                object.insert(name.clone(), to_json(value)?);
            }
            Value::Object(object)
        }
        Val::Variant(case, payload) => {
            let mut object = Map::new();
            object.insert("case".into(), Value::String(case.clone()));
            if let Some(p) = payload {
                object.insert("value".into(), to_json(p)?);
            }
            Value::Object(object)
        }
        Val::Enum(name) => Value::String(name.clone()),
        Val::Flags(names) => Value::Array(names.iter().cloned().map(Value::String).collect()),
        Val::Option(None) => Value::Null,
        Val::Option(Some(v)) => to_json(v)?,
        Val::Result(r) => {
            let (side, payload) = match r {
                Ok(p) => ("ok", p),
                Err(p) => ("err", p),
            };
            let mut object = Map::new();
            object.insert(
                side.into(),
                payload.as_ref().map_or(Ok(Value::Null), |p| to_json(p))?,
            );
            Value::Object(object)
        }
        Val::Float32(_) | Val::Float64(_) => return Err(unsupported("floating point")),
        _ => return Err(unsupported("resource or stream")),
    })
}

/// A rough upper bound of the bytes a value occupies, checked **before** converting it, so a huge
/// result is refused without building (and escaping) a huge JSON text.
pub(crate) fn size_hint(val: &Val) -> usize {
    match val {
        Val::String(s) => s.len(),
        Val::List(items) | Val::Tuple(items) => {
            items.iter().map(size_hint).sum::<usize>() + items.len()
        }
        Val::Record(fields) => fields.iter().map(|(n, v)| n.len() + size_hint(v)).sum(),
        Val::Variant(case, payload) => case.len() + payload.as_ref().map_or(0, |p| size_hint(p)),
        Val::Option(Some(v)) => size_hint(v),
        Val::Result(Ok(Some(v)) | Err(Some(v))) => size_hint(v),
        Val::Flags(names) => names.iter().map(String::len).sum(),
        _ => 8,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;
    use wasmtime::component::types::ComponentItem;
    use wasmtime::{Config, Engine};

    use super::*;

    /// The parameter types of a function imported by a component that declares them: a cheap way
    /// to get `Type` values without writing guest code.
    fn types() -> Vec<(String, Type)> {
        let text = r#"
        (component
          (type $point_def (record (field "x" s32) (field "y" s32) (field "label" string)))
          (import "point" (type $point (eq $point_def)))
          (type $shape_def (variant (case "dot") (case "circle" u32) (case "poly" (list $point))))
          (import "shape" (type $shape (eq $shape_def)))
          (type $colour_def (enum "red" "green"))
          (import "colour" (type $colour (eq $colour_def)))
          (type $perm_def (flags "read" "write"))
          (import "perm" (type $perm (eq $perm_def)))
          (import "f" (func
            (param "flag" bool) (param "small" s8) (param "byte" u8) (param "wide" s64) (param "huge" u64)
            (param "ch" char) (param "text" string) (param "xs" (list u16)) (param "pair" (tuple string u32))
            (param "p" $point) (param "s" $shape) (param "c" $colour) (param "perm" $perm)
            (param "maybe" (option string)) (param "r" (result u32 (error string))) (param "unit" (result (error string))))))
        "#;
        let engine = Engine::new(Config::new().wasm_component_model(true)).unwrap();
        let component =
            wasmtime::component::Component::new(&engine, wat::parse_str(text).unwrap()).unwrap();
        let ty = component.component_type();
        let imports: Vec<_> = ty.imports(&engine).collect();
        let (_, item) = imports.iter().find(|(name, _)| *name == "f").unwrap();
        let ComponentItem::ComponentFunc(func) = &item.ty else {
            panic!("f is not a function")
        };
        func.params()
            .map(|(name, ty)| (name.to_owned(), ty))
            .collect()
    }

    fn ty(name: &str) -> Type {
        types().into_iter().find(|(n, _)| n == name).unwrap().1
    }

    fn round_trip(name: &str, value: Value) {
        let val = to_val(&value, &ty(name), name).unwrap();
        assert_eq!(to_json(&val).unwrap(), value, "{name}");
    }

    fn rejected(name: &str, value: Value, needle: &str) {
        let err = to_val(&value, &ty(name), name).unwrap_err();
        assert_eq!(err.code(), "wasm.bad_arguments");
        assert!(err.to_string().contains(needle), "{name}: {err}");
    }

    #[test]
    fn every_supported_type_round_trips() {
        round_trip("flag", json!(true));
        round_trip("small", json!(-128));
        round_trip("byte", json!(255));
        round_trip("wide", json!(i64::MIN));
        round_trip("huge", json!(u64::MAX));
        round_trip("ch", json!("é"));
        round_trip("text", json!("hello ✓"));
        round_trip("xs", json!([0, 1, 65535]));
        round_trip("pair", json!(["a", 7]));
        round_trip("p", json!({"x": -1, "y": 2, "label": "origin"}));
        round_trip("s", json!({"case": "dot"}));
        round_trip("s", json!({"case": "circle", "value": 5}));
        round_trip(
            "s",
            json!({"case": "poly", "value": [{"x": 0, "y": 0, "label": "a"}]}),
        );
        round_trip("c", json!("green"));
        round_trip("perm", json!(["read", "write"]));
        round_trip("maybe", json!(null));
        round_trip("maybe", json!("here"));
        round_trip("r", json!({"ok": 3}));
        round_trip("r", json!({"err": "no"}));
        round_trip("unit", json!({"err": "no"}));
    }

    #[test]
    fn bad_arguments_are_refused_with_the_path() {
        rejected("flag", json!(1), "expected true or false");
        rejected("small", json!(128), "from -128 to 127");
        rejected("byte", json!(-1), "from 0 to 255");
        rejected(
            "byte",
            serde_json::from_str("1.5").unwrap(),
            "expected an integer",
        );
        rejected("huge", json!(-1), "from 0 to");
        rejected("ch", json!("ab"), "one-character");
        rejected("text", json!(null), "expected a string");
        rejected("xs", json!([1, "two"]), "xs[1]");
        rejected("pair", json!(["a"]), "expected 2 elements, found 1");
        rejected("p", json!({"x": 1, "y": 2}), "missing field `label`");
        rejected(
            "p",
            json!({"x": 1, "y": 2, "label": "l", "z": 3}),
            "unknown field `z`",
        );
        rejected("p", json!({"x": 1, "y": "two", "label": "l"}), "p.y");
        rejected("s", json!({"case": "square"}), "unknown case `square`");
        rejected("s", json!({"case": "circle"}), "needs a `value`");
        rejected("s", json!({"case": "dot", "value": 1}), "has no payload");
        rejected("c", json!("blue"), "is not one of red, green");
        rejected("perm", json!(["exec"]), "unknown flag `exec`");
        rejected("r", json!({"ok": 1, "err": "x"}), "exactly one of");
        rejected("r", json!({}), "exactly one of");
    }

    #[test]
    fn results_and_sizes() {
        let nothing = Val::Result(Ok(None));
        assert_eq!(to_json(&nothing).unwrap(), json!({"ok": null}));
        assert_eq!(size_hint(&Val::String("abcd".into())), 4);
        assert!(size_hint(&Val::List(vec![Val::String("ab".into()); 3])) >= 6);
    }
}

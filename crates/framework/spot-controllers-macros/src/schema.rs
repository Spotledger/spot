//! JSON Schema expressions for the types a whitelisted method takes and returns.
//!
//! The macro only sees tokens, never the fields of a type, so the schema of a built-in shape
//! is written out here and the schema of any other type is asked of it:
//! `<Type>::method_schema()`, which `#[derive(MethodSchema)]` generates.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{GenericArgument, PathArguments, Type};

/// An expression of type `serde_json::Value`: the JSON Schema of `ty`.
pub fn schema_expr(ty: &Type) -> TokenStream {
    match ty {
        Type::Tuple(t) if t.elems.is_empty() => quote!(::serde_json::json!({ "type": "null" })),
        Type::Reference(r) => schema_expr(&r.elem),
        Type::Paren(p) => schema_expr(&p.elem),
        Type::Group(g) => schema_expr(&g.elem),
        Type::Slice(s) => array_of(&schema_expr(&s.elem)),
        Type::Array(a) => array_of(&schema_expr(&a.elem)),
        Type::Path(p) if p.qself.is_none() => path_schema(ty, p),
        other => quote_spanned_call(other),
    }
}

fn array_of(items: &TokenStream) -> TokenStream {
    quote!(::serde_json::json!({ "type": "array", "items": #items }))
}

/// `<Type>::method_schema()`, spanned on the type so a missing derive points at it.
fn quote_spanned_call(ty: &Type) -> TokenStream {
    quote::quote_spanned!(syn::spanned::Spanned::span(ty) => <#ty>::method_schema())
}

fn first_arg(seg: &syn::PathSegment) -> Option<&Type> {
    match &seg.arguments {
        PathArguments::AngleBracketed(a) => a.args.iter().find_map(|g| match g {
            GenericArgument::Type(t) => Some(t),
            _ => None,
        }),
        _ => None,
    }
}

fn second_arg(seg: &syn::PathSegment) -> Option<&Type> {
    match &seg.arguments {
        PathArguments::AngleBracketed(a) => a
            .args
            .iter()
            .filter_map(|g| match g {
                GenericArgument::Type(t) => Some(t),
                _ => None,
            })
            .nth(1),
        _ => None,
    }
}

fn path_schema(ty: &Type, p: &syn::TypePath) -> TokenStream {
    let Some(seg) = p.path.segments.last() else {
        return quote_spanned_call(ty);
    };
    let name = seg.ident.to_string();
    match name.as_str() {
        "String" | "str" | "char" => quote!(::serde_json::json!({ "type": "string" })),
        "bool" => quote!(::serde_json::json!({ "type": "boolean" })),
        "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64" | "u128"
        | "usize" => quote!(::serde_json::json!({ "type": "integer" })),
        "f32" | "f64" => quote!(::serde_json::json!({ "type": "number" })),
        "Decimal" => {
            quote!(::serde_json::json!({ "type": "string", "format": "decimal" }))
        }
        "Date" => quote!(::serde_json::json!({ "type": "string", "format": "date" })),
        "OffsetDateTime" | "PrimitiveDateTime" => {
            quote!(::serde_json::json!({ "type": "string", "format": "date-time" }))
        }
        "Time" => quote!(::serde_json::json!({ "type": "string", "format": "time" })),
        // `serde_json::Value`: anything.
        "Value" | "Json" => quote!(::serde_json::json!({})),
        "Option" => match first_arg(seg) {
            Some(inner) => {
                let inner = schema_expr(inner);
                quote!(::serde_json::json!({ "anyOf": [#inner, { "type": "null" }] }))
            }
            None => quote_spanned_call(ty),
        },
        "Vec" | "VecDeque" | "HashSet" | "BTreeSet" => match first_arg(seg) {
            Some(inner) => array_of(&schema_expr(inner)),
            None => quote_spanned_call(ty),
        },
        "Box" | "Arc" | "Rc" | "Cow" => match first_arg(seg) {
            Some(inner) => schema_expr(inner),
            None => quote_spanned_call(ty),
        },
        "HashMap" | "BTreeMap" => match second_arg(seg) {
            Some(value) => {
                let value = schema_expr(value);
                quote!(::serde_json::json!({ "type": "object", "additionalProperties": #value }))
            }
            None => quote_spanned_call(ty),
        },
        _ => quote_spanned_call(ty),
    }
}

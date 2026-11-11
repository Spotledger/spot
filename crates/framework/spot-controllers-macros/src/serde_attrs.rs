//! The few `#[serde(...)]` attributes the schema derive understands.

use syn::{Attribute, Error, Expr, ExprLit, Lit, Token};

/// What a field, variant or container says through `#[serde(...)]`.
#[derive(Default)]
pub struct SerdeAttrs {
    /// `rename = "x"`.
    pub rename: Option<String>,
    /// `default` or `default = "path"`: the property may be absent.
    pub default: bool,
    /// `skip`, `skip_deserializing`: the input never carries it.
    pub skip: bool,
    /// `skip_serializing_if`: the property may be absent from output.
    pub skip_serializing_if: bool,
    /// `deny_unknown_fields` on the container.
    pub deny_unknown_fields: bool,
}

/// Keys whose effect the schema could not follow, so the derive refuses them.
const UNSUPPORTED: &[&str] = &[
    "rename_all",
    "rename_all_fields",
    "flatten",
    "tag",
    "content",
    "untagged",
    "from",
    "into",
    "try_from",
    "with",
    "deserialize_with",
    "serialize_with",
    "transparent",
    "alias",
    "other",
];

/// Reads the `#[serde(...)]` attributes of one item.
pub fn parse(attrs: &[Attribute]) -> syn::Result<SerdeAttrs> {
    let mut out = SerdeAttrs::default();
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            if UNSUPPORTED.contains(&key.as_str()) {
                return Err(meta.error(format!(
                    "`#[serde({key})]` is not supported by #[derive(MethodSchema)]: the schema \
                     could not match what serde reads; write the argument type with plain \
                     named fields"
                )));
            }
            match key.as_str() {
                "rename" => {
                    let value: Expr = meta.value()?.parse()?;
                    match value {
                        Expr::Lit(ExprLit {
                            lit: Lit::Str(s), ..
                        }) => out.rename = Some(s.value()),
                        other => {
                            return Err(Error::new_spanned(
                                other,
                                "write `rename = \"name\"`; split renames are not supported",
                            ));
                        }
                    }
                }
                "default" => {
                    out.default = true;
                    if meta.input.peek(Token![=]) {
                        let _: Expr = meta.value()?.parse()?;
                    }
                }
                "skip" | "skip_deserializing" => out.skip = true,
                "skip_serializing_if" => {
                    out.skip_serializing_if = true;
                    let _: Expr = meta.value()?.parse()?;
                }
                "deny_unknown_fields" => out.deny_unknown_fields = true,
                _ => {
                    if meta.input.peek(Token![=]) {
                        let _: Expr = meta.value()?.parse()?;
                    }
                }
            }
            Ok(())
        })?;
    }
    Ok(out)
}

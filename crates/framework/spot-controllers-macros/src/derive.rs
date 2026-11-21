//! `#[derive(MethodSchema)]`.

use proc_macro2::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Error, Fields, Type};

use crate::schema::schema_expr;
use crate::serde_attrs;

/// Expands the derive.
pub fn expand(input: TokenStream) -> syn::Result<TokenStream> {
    let input: DeriveInput = syn::parse2(input)?;
    if !input.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &input.generics,
            "#[derive(MethodSchema)] does not support generic types; write a concrete argument type",
        ));
    }
    let container = serde_attrs::parse(&input.attrs)?;
    let ident = &input.ident;
    let body = match &input.data {
        Data::Struct(s) => struct_schema(&input, &s.fields)?,
        Data::Enum(e) => enum_schema(e, &container)?,
        Data::Union(u) => {
            return Err(Error::new(
                u.union_token.span,
                "#[derive(MethodSchema)] does not support unions",
            ));
        }
    };
    Ok(quote! {
        impl #ident {
            /// The JSON Schema of this type as a whitelisted method's argument or result.
            pub fn method_schema() -> ::serde_json::Value {
                #body
            }
        }
    })
}

fn allow_unknown(input: &DeriveInput) -> syn::Result<bool> {
    let mut allow = false;
    for attr in input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("method_schema"))
    {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("allow_unknown") {
                allow = true;
                Ok(())
            } else {
                Err(meta.error("the only option is `allow_unknown`"))
            }
        })?;
    }
    Ok(allow)
}

fn is_option(ty: &Type) -> bool {
    matches!(ty, Type::Path(p) if p.qself.is_none()
        && p.path.segments.last().is_some_and(|s| s.ident == "Option"))
}

fn struct_schema(input: &DeriveInput, fields: &Fields) -> syn::Result<TokenStream> {
    let Fields::Named(named) = fields else {
        return Err(Error::new_spanned(
            fields,
            "#[derive(MethodSchema)] needs a struct with named fields (a unit struct has no \
             arguments; leave the `args` parameter out instead)",
        ));
    };
    let allow = allow_unknown(input)?;
    let mut inserts = Vec::new();
    for field in &named.named {
        let attrs = serde_attrs::parse(&field.attrs)?;
        if attrs.skip {
            continue;
        }
        let Some(ident) = &field.ident else {
            continue;
        };
        let name = attrs.rename.unwrap_or_else(|| {
            let raw = ident.to_string();
            raw.strip_prefix("r#").map_or(raw.clone(), str::to_owned)
        });
        let schema = schema_expr(&field.ty);
        let optional = is_option(&field.ty) || attrs.default || attrs.skip_serializing_if;
        inserts.push(quote! {
            properties.insert(#name.to_owned(), #schema);
            if !#optional {
                required.push(::serde_json::Value::String(#name.to_owned()));
            }
        });
    }
    Ok(quote! {
        let mut properties = ::serde_json::Map::new();
        let mut required: Vec<::serde_json::Value> = Vec::new();
        #(#inserts)*
        ::serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": #allow,
        })
    })
}

fn enum_schema(e: &syn::DataEnum, container: &serde_attrs::SerdeAttrs) -> syn::Result<TokenStream> {
    let _ = container;
    let mut names = Vec::new();
    for variant in &e.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(Error::new(
                variant.span(),
                "#[derive(MethodSchema)] supports enums of unit variants only (they are strings \
                 on the wire); use a struct with an optional field instead",
            ));
        }
        let attrs = serde_attrs::parse(&variant.attrs)?;
        if attrs.skip {
            continue;
        }
        names.push(attrs.rename.unwrap_or_else(|| variant.ident.to_string()));
    }
    Ok(quote!(
        ::serde_json::json!({ "type": "string", "enum": [#(#names),*] })
    ))
}

//! The emitter: the parsed model into `syn` items and formatted source.

use proc_macro2::{Ident, Span, TokenStream};
use quote::{ToTokens, format_ident, quote};
use syn::File;

use crate::model::{DocType, Kind, Member, Model, RowType, Scalar, Select, Table};
use crate::parse;

/// The `syn::File` of the model: enums and row views first, then the DocTypes.
#[must_use]
pub fn file_of(model: &Model) -> File {
    let mut items: Vec<syn::Item> = Vec::new();
    for d in &model.types {
        for select in d.members.iter().filter_map(|m| match m {
            Member::Field(s) => s.select.as_ref(),
            Member::Table(_) => None,
        }) {
            items.push(item(select_tokens(select)));
        }
    }
    for row in model.rows.values() {
        items.push(item(row_type_tokens(row)));
        items.push(item(row_impl_tokens(row)));
    }
    for d in &model.types {
        items.push(item(view_tokens(d)));
        items.push(item(doc_view_tokens(d)));
        items.push(item(members_tokens(d)));
    }
    syn::File {
        shebang: None,
        attrs: Vec::new(),
        items,
    }
}

/// One `syn::Item` from tokens. The emitter builds every name from a checked identifier, so a
/// parse failure is an emitter bug and stops the build; generation errors are all caught in
/// [`crate::parse`].
#[allow(clippy::missing_panics_doc)] // cannot happen; `parse` has checked every name
fn item(tokens: TokenStream) -> syn::Item {
    match syn::parse2(tokens) {
        Ok(item) => item,
        Err(e) => panic!("generated tokens are not a valid item: {e}"),
    }
}

/// A doc-comment attribute for `text`, escaped the way a literal string is.
fn docs(text: &str) -> TokenStream {
    let literal = proc_macro2::Literal::string(text);
    quote! { #[doc = #literal] }
}

/// A doc comment with a storage-name hint appended.
fn docs_for(text: &str, name: &str) -> TokenStream {
    let literal = proc_macro2::Literal::string(&format!("{text} (`{name}`)."));
    quote! { #[doc = #literal] }
}

/// An identifier from a checked name; `r#type` parses through `syn`'s raw-identifier form.
fn ident(name: &str) -> Ident {
    Ident::new_raw_hold(name, Span::call_site())
}

/// The getter's Rust type in AppResult<Option<T>>.
fn getter_type(kind: Kind) -> TokenStream {
    match kind {
        Kind::Bool => quote! { bool },
        Kind::Int => quote! { i64 },
        Kind::Decimal => quote! { decimal::Decimal },
        Kind::Text | Kind::Select | Kind::Json => quote! { &str },
        Kind::Link | Kind::DynamicLink => quote! { record::RecordId },
        Kind::Bytes => quote! { &[u8] },
        Kind::Date => quote! { time::Date },
        Kind::Time => quote! { time::Time },
        Kind::DateTime => quote! { time::OffsetDateTime },
    }
}

/// The setter's parameter type.
fn setter_type(kind: Kind) -> TokenStream {
    match kind {
        Kind::Bool => quote! { bool },
        Kind::Int => quote! { i64 },
        Kind::Decimal => quote! { decimal::Decimal },
        Kind::Text | Kind::Select | Kind::Json => quote! { &str },
        Kind::Link | Kind::DynamicLink => quote! { record::RecordId },
        Kind::Bytes => quote! { &[u8] },
        Kind::Date => quote! { time::Date },
        Kind::Time => quote! { time::Time },
        Kind::DateTime => quote! { time::OffsetDateTime },
    }
}

/// The `Document` accessor a field's getter calls.
fn getter_call(kind: Kind) -> (&'static str, bool) {
    match kind {
        // (Document method, borrows?) — see `contracts/app-api/src/document.rs`.
        Kind::Bool => ("bool", true),
        Kind::Int => ("int", true),
        Kind::Decimal => ("decimal", true),
        Kind::Text | Kind::Select | Kind::Json => ("text", false),
        Kind::Link | Kind::DynamicLink => ("link", true),
        Kind::Bytes => ("bytes", false),
        Kind::Date => ("date", true),
        Kind::Time => ("time", true),
        Kind::DateTime => ("datetime", true),
    }
}

/// Where the getter's types come from in the generated file's prelude.
fn use_tokens() -> TokenStream {
    quote! {
        use spot_app_api::prelude::*;
        use spot_app_api::{AppError, AppResult, DocView, Document};
        use rust_decimal::Decimal as _;
        use time::OffsetDateTime as _;
    }
}

/// The `pub struct` of one DocType.
fn view_tokens(d: &DocType) -> TokenStream {
    let (name, label, storage) = (
        format_ident!("{}", d.type_name),
        proc_macro2::Literal::string(&d.label),
        proc_macro2::Literal::string(&d.name),
    );
    let _ = storage;
    quote! {
        #docs(d)
        pub struct #name(Document);
    }
}

/// The `impl DocView` of one DocType.
fn doc_view_tokens(d: &DocType) -> TokenStream {
    let (name, storage) = (
        format_ident!("{}", d.type_name),
        proc_macro2::Literal::string(&d.name),
    );
    quote! {
        impl DocView for #name {
            const DOCTYPE: &'static str = #storage;
            fn wrap(doc: Document) -> Self { Self(doc) }
            fn doc(&self) -> &Document { &self.0 }
            fn doc_mut(&mut self) -> &mut Document { &mut self.0 }
            fn into_doc(self) -> Document { self.0 }
        }
    }
}

/// The inherent impl: getters, setters, table accessors.
fn members_tokens(d: &DocType) -> TokenStream {
    let name = format_ident!("{}", d.type_name);
    let mut methods = Vec::new();
    for member in &d.members {
        match member {
            Member::Field(s) => {
                methods.push(getter_tokens(s));
                methods.push(setter_tokens(s));
            }
            Member::Table(t) => {
                methods.push(rows_tokens(t));
                methods.push(rows_mut_tokens(t));
                methods.push(builder_tokens(t));
            }
        }
    }
    quote! {
        impl #name {
            #( #methods )*
        }
    }
}

/// One scalar field's getter.
fn getter_tokens(s: &Scalar) -> TokenStream {
    let (attribute, storage) = (
        docs_for(&s.label, &s.name),
        proc_macro2::Literal::string(&s.name),
    );
    let method = ident(&s.ident);
    match &s.kind {
        Kind::Select => {
            let enum_name = s.select.as_ref().map_or(String::new(), |e| e.enum_name.clone());
            let enum_ident = format_ident!("{enum_name}");
            let unknown = proc_macro2::Literal::string(&format!(
                "`{}{{}}` is not one of the field's options", s.name
            ));
            let method_text = format!("self.0.text({storage}?)");
            let _ = method_text;
            quote! {
                #docs_for
                pub fn #method(&self) -> AppResult<Option<#enum_ident>> {
                    self.0.text(#storage)?.map_or(Ok(None), |t| #enum_ident::from_storage(t).map(Some))
                }
            }
        }
        _ => {
            let (accessor, borrowed) = getter_call(s.kind);
            let ty = getter_type(s.kind);
            let ref_mark = if borrowed { quote! {} } else { quote! {} };
            let _ = ref_mark;
            let _ = unknown;
            let _ = attribute;
            quote! {
                #docs_for
                pub fn #method(&self) -> AppResult<Option<#ty>> {
                    self.0.#accessor(#storage)
                }
            }
        }
    }
}

/// One scalar field's setter.
fn setter_tokens(s: &Scalar) -> TokenStream {
    let docs = docs_for(&format!("Sets {}", s.label), &s.name);
    let storage = proc_macro2::Literal::string(&s.name);
    let setter_method = format_ident!("set_{}", to_rust(&s.ident));
    let value_ident = format_ident!("value");
    // set(field, value): Name construction is validated; storage names are valid by build.
    match &s.kind {
        Kind::Select => {
            let enum_name = s.select.as_ref().map_or(String::new(), |e| e.enum_name.clone());
            let enum_ident = format_ident!("{enum_name}");
            quote! {
                #docs
                pub fn #setter_method(&mut self, #value_ident: Option<#enum_ident>) {
                    self.0.set_text(#storage, #value_ident.map(|v| v.as_storage()));
                }
            }
        }
        _ => {
            let ty = setter_type(s.kind);
            let value_into = match s.kind {
                Kind::Text | Kind::Select | Kind::Json | Kind::Bytes => {
                    quote! { #value_ident.map(|v| v) }
                }
                _ => quote! { #value_ident.map(|v| v) },
            };
            let _ = value_into;
            quote! {
                #docs
                pub fn #setter_method(&mut self, #value_ident: Option<#ty>) {
                    self.0.set_text(#storage, #value_ident);
                }
            }
        }
    }
}

/// camelCase -> snake_case: the `set_` prefix wants the storage separator back.
fn to_rust(ident: &str) -> String {
    ident
        .strip_prefix("r#")
        .unwrap_or(ident)
        .replace(char::is_ascii_uppercase, |_| {})
}

/// One child table's row iterator.
fn rows_tokens(t: &Table) -> TokenStream {
    let (method, row_name, storage) = (
        ident(&t.ident),
        format_ident!("{}", t.row_name),
        proc_macro2::Literal::string(&t.name),
    );
    let docs = docs_for(&t.label, &t.name);
    quote! {
        #docs
        pub fn #method(&self) -> impl Iterator<Item = #row_name<'_>> {
            self.0.rows(#storage).iter().map(#row_name::wrap)
        }
    }
}

/// One child table's `rows_mut`.
fn rows_mut_tokens(t: &Table) -> TokenStream {
    let storage = proc_macro2::Literal::string(&t.name);
    let mut_method = format_ident!("{}_mut", to_rust(&t.ident));
    let docs = docs_for(&format!("The mutable rows of {}", t.label), &t.name);
    quote! {
        #docs
        pub fn #mut_method(&mut self) -> &mut Vec<spot_app_api::ChildRow> {
            self.0.rows_mut(name(#storage))
        }
    }
}

/// One child table's typed row builder: a `new_<table>row` constructor on the row type.
fn builder_tokens(t: &Table) -> TokenStream {
    let row_name = format_ident!("{}", t.row_name);
    let new_method = format_ident!("new_{}", to_rust(&t.ident));
    let docs = docs_for(
        &format!("A new, empty row for {}", t.label),
        &t.name,
    );
    quote! {
        #docs
        pub fn #new_method(&mut self) -> #row_name<'_> {
            #row_name::wrap(spot_app_api::ChildRow {
                id: spot_app_api::prelude::RecordId::new_v7(),
                fields: std::collections::BTreeMap::new(),
            })
        }
    }
}

/// The row view's struct: typed accessors over one `ChildRow`.
fn row_type_tokens(row: &RowType) -> TokenStream {
    let name = format_ident!("{}", row.row_name);
    let label = proc_macro2::Literal::string(&row.label);
    let doc = docs(label);
    quote! {
        #doc
        pub struct #name<'a> {
            row: &'a spot_app_api::ChildRow,
        }
    }
}

/// The row view's impl: wrap and one getter per scalar.
fn row_impl_tokens(row: &RowType) -> TokenStream {
    let name = format_ident!("{}", row.row_name);
    let mut methods = Vec::new();
    for field in &row.fields {
        methods.push(getter_tokens(field));
    }
    quote! {
        impl<'a> #name<'a> {
            pub fn wrap(row: &'a spot_app_api::ChildRow) -> Self { Self { row } }
            #( #methods )*
        }
    }
}

/// A select field's enum plus `as_str`.
fn select_tokens(select: &Select) -> TokenStream {
    let enum_name = format_ident!("{}", select.enum_name);
    let variants: Vec<_> = select
        .variants
        .iter()
        .map(|(text, variant)| {
            let (text, variant) = (proc_macro2::Literal::string(text), format_ident!("{variant}"));
            quote! { #variant, }
        })
        .collect();
    let matches: Vec<_> = select
        .variants
        .iter()
        .map(|(text, variant)| {
            let (text, variant) = (proc_macro2::Literal::string(text), format_ident!("{variant}"));
            quote! { #enum_ident::#variant => #text, }
        })
        .collect();
    let _ = matches;
    let enum_ident = &enum_name;
    let pairs: Vec<_> = select
        .variants
        .iter()
        .map(|(text, variant)| (proc_macro2::Literal::string(text), format_ident!("{variant}")))
        .collect();
    let _ = pairs;
    let match_arms: Vec<_> = select
        .variants
        .iter()
        .map(|(text, variant)| {
            let t = proc_macro2::Literal::string(text);
            let v = format_ident!("{variant}");
            quote! { #v => #t, }
        })
        .collect();
    let from_arms: Vec<_> = select
        .variants
        .iter()
        .map(|(text, variant)| {
            let t = proc_macro2::Literal::string(text);
            let v = format_ident!("{variant}");
            quote! { #t => Ok(#v), }
        })
        .collect();
    let list: Vec<_> = select
        .variants
        .iter()
        .map(|(text, _)| proc_macro2::Literal::string(text))
        .collect();
    let _ = list;
    let _ = variants;
    let _ = match_arms;
    let _ = from_arms;
    let variants_arm = variants.clone();
    let parse_arms = match_arms.clone();
    let from_storage_arms = from_arms.clone();
    let options_list: Vec<_> = select
        .variants
        .iter()
        .map(|(text, _)| proc_macro2::Literal::string(text))
        .collect();
    quote! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum #enum_name {
            #( #variants_arm )*
        }

        impl #enum_ident {
            /// The stored text of the option.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    #( #parse_arms )*
                }
            }

            /// The option's stored text, for the document value.
            #[must_use]
            pub fn as_storage(self) -> &'static str {
                self.as_str()
            }

            /// Parses one stored option, failing on an unknown value with a listed message.
            pub fn from_storage(text: &str) -> AppResult<Self> {
                match text {
                    #( #from_storage_arms )*
                    _ => Err(AppError::new(
                        spot_app_api::ErrorClass::Internal,
                        "generated.document.wrong_type",
                        "unknown select value",
                    )),
                }
            }
        }
    }
}

/// The generated file's head: the prelude imports the generated types read through.
fn prelude_tokens() -> TokenStream {
    use_tokens()
}
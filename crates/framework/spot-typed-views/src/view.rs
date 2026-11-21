//! The emitter: the parsed model into `syn` items, formatted by `prettyplease`.
//!
//! Every DocType emits three items (`struct`, `impl DocView`, inherent impl), every select
//! field one enum, every used child DocType one row view. Getters read through the typed
//! `Document` accessors (`text`, `decimal`, `int`, `link`) and, for the kinds those do not
//! cover, through small generated helpers, so every getter has the same
//! `AppResult<Option<T>>` shape; a generated view never copies data and never panics.
//!
//! `Document::set` takes a validated `Name`, so setters return `AppResult<()>`: the
//! storage name was checked at build time, but the generated code reports a failure as an
//! `Internal` error instead of panicking if a definition ever got out of step.
//!
//! Doc comments ride in `#[doc = "..."]` attributes. Each starts with one space
//! (`#[doc = " The …"]`), so `prettyplease` prints them as `/// The …` with the space
//! rustdoc's rendering expects.

use proc_macro2::{Ident, Literal, TokenStream};
use quote::{format_ident, quote};
use syn::File;

use crate::parse::{DocType, Kind, Member, Model, RowType, Scalar, Select, Table, lower_first};

/// The `syn::File` of the model: the prelude and helpers, then the select enums, then the
/// row views, then the DocTypes' views, in definition order.
#[must_use]
pub fn file_of(model: &Model) -> File {
    let mut items: Vec<syn::Item> = Vec::new();
    items.extend(items_of(prelude_tokens()));
    items.extend(items_of(helpers_tokens()));
    for d in &model.types {
        for select in d.members.iter().filter_map(|m| match m {
            Member::Field(s) => s.select_of(),
            Member::Table(_) => None,
        }) {
            items.extend(items_of(select_tokens(select)));
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

/// Every `syn::Item` from tokens: the prelude and the helpers hold many at once.
fn items_of(tokens: TokenStream) -> Vec<syn::Item> {
    match syn::parse2::<syn::File>(tokens) {
        Ok(file) => file.items,
        Err(e) => panic!("generated tokens are not valid items: {e}"),
    }
}

/// One `syn::Item` from tokens. Every name was checked in `parse`, so a parse failure here is
/// an emitter bug; generation errors surface in `parse` instead.
fn item(tokens: TokenStream) -> syn::Item {
    let mut parsed = items_of(tokens);
    match (parsed.len(), parsed.pop()) {
        (1, Some(item)) => item,
        _ => panic!("expected exactly one item per call"),
    }
}

/// A doc-comment attribute holding `text`, one leading space in (`/// ` rendering).
fn docs(text: &str) -> TokenStream {
    let literal = Literal::string(&format!(" {}", text.trim_start()));
    quote! { #[doc = #literal] }
}

/// A doc-comment attribute: the label, then the storage name in backticks.
fn docs_named(text: &str, name: &str) -> TokenStream {
    let literal = Literal::string(&format!(" {} (`{name}`).", text.trim_end()));
    quote! { #[doc = #literal] }
}

/// The method identifier; `parse` spelled keyword members `r#type`, which parses as-is.
fn method(ident: &str) -> Ident {
    raw_ident(ident)
}

/// `set_<field>`.
fn setter_ident(field: &str) -> Ident {
    raw_ident(&format!("set_{}", field.trim_start_matches("r#")))
}

/// `<table>_mut`.
fn table_mut_ident(field: &str) -> Ident {
    raw_ident(&format!("{}_mut", field.trim_start_matches("r#")))
}

/// `new_<table>`.
fn new_row_ident(field: &str) -> Ident {
    raw_ident(&format!("new_{}", field.trim_start_matches("r#")))
}

/// Parses an identifier that may carry the `r#` prefix; `parse` has checked every name.
fn raw_ident(name: &str) -> Ident {
    syn::parse_str::<TokenStream>(name)
        .ok()
        .and_then(|t| match t.into_iter().next() {
            Some(proc_macro2::TokenTree::Ident(i)) => Some(i),
            _ => None,
        })
        .unwrap_or_else(|| format_ident!("{name}"))
}

/// The generated file's head: one `use` through `spot_app_api` for every type the emitted
/// code names. The consuming crate depends only on `spot_app_api` (ADR-06), and rustc's
/// extern prelude carries only a crate's direct dependencies, so every type must ride the
/// one re-export path: `spot_app_api::prelude::*` re-spells `Decimal`, `Date`,
/// `OffsetDateTime`, `Time`, `Name`, `Value` and `RecordId`, and `spot_app_api` itself
/// re-exports `ChildRow`. Every line ends with a semicolon because `syn::File` parses `use`
/// items one by one.
fn prelude_tokens() -> TokenStream {
    quote! {
        use spot_app_api::prelude::Decimal;
        use spot_app_api::prelude::{Date, OffsetDateTime, Time};
        use spot_app_api::prelude::{Name, RecordId, Value};
        use spot_app_api::{AppError, AppResult, ChildRow, DocView, Document};
        use spot_app_api::error::ErrorClass;
        use std::collections::BTreeMap;
    }
}

/// The helpers the generated getters and setters call, emitted once.
fn helpers_tokens() -> TokenStream {
    quote! {
        /// The type-holding error of the generated getters: a programming error, not user input.
        fn wrong_type(field: &str) -> AppError {
            AppError::new(
                ErrorClass::Internal,
                "generated.document.wrong_type",
                format!("field `{field}` does not hold the type the view reads"),
            )
        }

        /// A storage name for a generated accessor: the build checked the spelling, so a
        /// failure here is an `Internal` error, never a panic.
        fn name_of(field: &str) -> AppResult<Name> {
            Name::new(field).map_err(|e| {
                AppError::new(
                    ErrorClass::Internal,
                    "generated.document.invalid_name",
                    e.to_string(),
                )
            })
        }

        /// One row field's value: `Null` when the row does not set the field.
        #[must_use]
        pub fn row_value_of<'v>(row: &'v ChildRow, field: &str) -> &'v Value {
            row.fields
                .iter()
                .find(|(k, _)| k.as_str() == field)
                .map_or(&Value::Null, |(_, v)| v)
        }

        /// A bool field: `Null` to `None`, otherwise typed, otherwise an error.
        pub fn as_bool_of(value: &Value, field: &str) -> AppResult<Option<bool>> {
            match value {
                Value::Null => Ok(None),
                Value::Bool(b) => Ok(Some(*b)),
                _ => Err(wrong_type(field)),
            }
        }
        /// A bytes field: `Null` to `None`, otherwise typed, otherwise an error.
        pub fn as_bytes_of<'v>(value: &'v Value, field: &str) -> AppResult<Option<&'v [u8]>> {
            match value {
                Value::Null => Ok(None),
                Value::Bytes(b) => Ok(Some(b.as_slice())),
                _ => Err(wrong_type(field)),
            }
        }

        /// A date field: `Null` to `None`, otherwise typed, otherwise an error.
        pub fn as_date_of(value: &Value, field: &str) -> AppResult<Option<Date>> {
            match value {
                Value::Null => Ok(None),
                Value::Date(d) => Ok(Some(*d)),
                _ => Err(wrong_type(field)),
            }
        }

        /// A time field: `Null` to `None`, otherwise typed, otherwise an error.
        pub fn as_time_of(value: &Value, field: &str) -> AppResult<Option<Time>> {
            match value {
                Value::Null => Ok(None),
                Value::Time(t) => Ok(Some(*t)),
                _ => Err(wrong_type(field)),
            }
        }

        /// A datetime field: `Null` to `None`, otherwise typed, otherwise an error.
        pub fn as_datetime_of(value: &Value, field: &str) -> AppResult<Option<OffsetDateTime>> {
            match value {
                Value::Null => Ok(None),
                Value::DateTime(dt) => Ok(Some(*dt)),
                _ => Err(wrong_type(field)),
            }
        }

        /// Sets one field: the value, or `Null`.
        pub fn generated_set(doc: &mut Document, field: &str, value: Value) -> AppResult<()> {
            doc.set(name_of(field)?, value);
            Ok(())
        }

        /// Appends one new row to a child table and returns it, for the row builders.
        pub fn generated_push_row<'d>(
            doc: &'d mut Document,
            table: &str,
            fields: std::collections::BTreeMap<Name, Value>,
        ) -> AppResult<&'d ChildRow> {
            let name = name_of(table)?;
            let rows = doc.rows_mut(name);
            rows.push(ChildRow {
                id: RecordId::new_v7(),
                fields,
            });
            rows.last().ok_or_else(|| {
                AppError::new(
                    ErrorClass::Internal,
                    "generated.document.missing_row",
                    format!("the row pushed to `{table}` is not in the table"),
                )
            })
        }
    }
}

/// The Rust type behind `AppResult<Option<T>>` for a getter.
fn getter_type(kind: Kind) -> TokenStream {
    match kind {
        Kind::Bool => quote! { bool },
        Kind::Int => quote! { i64 },
        Kind::Decimal => quote! { Decimal },
        Kind::Text | Kind::Select | Kind::Json => quote! { &str },
        Kind::Link | Kind::DynamicLink => quote! { RecordId },
        Kind::Bytes => quote! { &[u8] },
        Kind::Date => quote! { Date },
        Kind::Time => quote! { Time },
        Kind::DateTime => quote! { OffsetDateTime },
    }
}

/// The setter's parameter type: stored by copy or by borrow.
fn setter_type(kind: Kind) -> TokenStream {
    match kind {
        Kind::Bool => quote! { bool },
        Kind::Int => quote! { i64 },
        Kind::Decimal => quote! { Decimal },
        Kind::Text | Kind::Select | Kind::Json => quote! { &str },
        Kind::Link | Kind::DynamicLink => quote! { RecordId },
        Kind::Bytes => quote! { Vec<u8> },
        Kind::Date => quote! { Date },
        Kind::Time => quote! { Time },
        Kind::DateTime => quote! { OffsetDateTime },
    }
}

/// The `Document` accessor a getter calls, for the kinds it has typed methods for.
fn document_accessor(kind: Kind) -> Option<TokenStream> {
    match kind {
        Kind::Decimal => Some(quote! { decimal }),
        Kind::Int => Some(quote! { int }),
        Kind::Text | Kind::Select | Kind::Json => Some(quote! { text }),
        Kind::Link | Kind::DynamicLink => Some(quote! { link }),
        _ => None,
    }
}

/// The generated helper a getter calls, for the kinds `Document` has no typed method for.
/// Never silent: the fallback would type-check a Decimal field as text.
fn generated_accessor(kind: Kind) -> TokenStream {
    match kind {
        Kind::Bool => quote! { as_bool_of },
        Kind::Bytes => quote! { as_bytes_of },
        Kind::Date => quote! { as_date_of },
        Kind::Time => quote! { as_time_of },
        Kind::DateTime => quote! { as_datetime_of },
        Kind::Decimal
        | Kind::Int
        | Kind::Text
        | Kind::Select
        | Kind::Json
        | Kind::Link
        | Kind::DynamicLink => quote! { as_text_of },
    }
}

/// The `Value` construction a setter stores: `Some(v)` is the field's variant.
fn value_construct(kind: Kind) -> TokenStream {
    match kind {
        Kind::Bool => quote! { Value::Bool(v) },
        Kind::Int => quote! { Value::Int(v) },
        Kind::Decimal => quote! { Value::Decimal(v) },
        Kind::Text | Kind::Select | Kind::Json => quote! { Value::Text(v.to_owned()) },
        Kind::Link | Kind::DynamicLink => quote! { Value::Id(v) },
        Kind::Bytes => quote! { Value::Bytes(v) },
        Kind::Date => quote! { Value::Date(v) },
        Kind::Time => quote! { Value::Time(v) },
        Kind::DateTime => quote! { Value::DateTime(v) },
    }
}

/// The `pub struct <Name>(Document);` of one DocType.
fn view_tokens(d: &DocType) -> TokenStream {
    let name = format_ident!("{}", d.type_name);
    let doc = docs_named(
        &format!(
            "The typed view of the {} DocType{}",
            d.label,
            if d.is_posted { " (posted)" } else { "" }
        ),
        &d.name,
    );
    quote! {
        #doc
        pub struct #name(Document);
    }
}

/// The `impl DocView` of one DocType.
fn doc_view_tokens(d: &DocType) -> TokenStream {
    let name = format_ident!("{}", d.type_name);
    let storage = Literal::string(&d.name);
    quote! {
        impl DocView for #name {
            const DOCTYPE: &'static str = #storage;

            fn wrap(doc: Document) -> Self {
                Self(doc)
            }

            fn doc(&self) -> &Document {
                &self.0
            }

            fn doc_mut(&mut self) -> &mut Document {
                &mut self.0
            }

            fn into_doc(self) -> Document {
                self.0
            }
        }
    }
}

/// The inherent impl of one DocType: getters, setters and table accessors.
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
                methods.push(row_builder_tokens(t));
            }
        }
    }
    quote! {
        impl #name {
            #( #methods )*
        }
    }
}

/// One scalar field's getter body: the `Document` accessor, a select's enum parse, or the
/// generated helper.
fn getter_body(s: &Scalar) -> TokenStream {
    let storage = Literal::string(&s.name);
    if let Kind::Select = s.kind {
        let enum_name = s
            .select_of()
            .map_or_else(String::new, |e| e.enum_name.clone());
        let enum_ident = format_ident!("{enum_name}");
        return quote! {
            match self.0.text(#storage) {
                Ok(None) => Ok(None),
                Ok(Some(t)) => #enum_ident::parse(t).map(Some),
                Err(e) => Err(e),
            }
        };
    }
    match document_accessor(s.kind) {
        Some(accessor) => quote! { self.0.#accessor(#storage) },
        None => {
            let helper = generated_accessor(s.kind);
            quote! { #helper(self.0.get(#storage), #storage) }
        }
    }
}

/// One scalar field's getter.
fn getter_tokens(s: &Scalar) -> TokenStream {
    let method = method(&s.ident);
    let doc = docs(&s.doc_comment());
    let body = getter_body(s);
    let returned = if let Kind::Select = s.kind {
        let enum_name = s
            .select_of()
            .map_or_else(String::new, |e| e.enum_name.clone());
        let enum_ident = format_ident!("{enum_name}");
        quote! { Option<#enum_ident> }
    } else {
        let ty = getter_type(s.kind);
        quote! { Option<#ty> }
    };
    quote! {
        #doc
        pub fn #method(&self) -> AppResult<#returned> {
            #body
        }
    }
}

/// One scalar field's setter.
fn setter_tokens(s: &Scalar) -> TokenStream {
    let setter = setter_ident(&s.ident);
    let setter_doc = docs(&s.setter_comment());
    let storage = Literal::string(&s.name);
    let value = format_ident!("value");
    let ty = setter_type(s.kind);
    match &s.kind {
        Kind::Select => {
            let enum_name = s
                .select_of()
                .map_or_else(String::new, |e| e.enum_name.clone());
            let enum_ident = format_ident!("{enum_name}");
            quote! {
                #setter_doc
                pub fn #setter(&mut self, #value: Option<#enum_ident>) -> AppResult<()> {
                    match #value {
                        Some(v) => generated_set(&mut self.0, #storage, Value::Text(v.as_str().to_owned())),
                        None => generated_set(&mut self.0, #storage, Value::Null),
                    }
                }
            }
        }
        _ => {
            let construct = value_construct(s.kind);
            quote! {
                #setter_doc
                pub fn #setter(&mut self, #value: Option<#ty>) -> AppResult<()> {
                    match #value {
                        Some(v) => generated_set(&mut self.0, #storage, #construct),
                        None => generated_set(&mut self.0, #storage, Value::Null),
                    }
                }
            }
        }
    }
}

/// One child table's typed row iterator.
fn rows_tokens(t: &Table) -> TokenStream {
    let method = method(&t.ident);
    let row_name = format_ident!("{}", t.row_name);
    let storage = Literal::string(&t.name);
    let doc = docs(&format!(
        "An iterator over the typed rows of the {} child table (`{}`).",
        lower_first(&t.label),
        t.name
    ));
    quote! {
        #doc
        pub fn #method(&self) -> impl Iterator<Item = #row_name<'_>> {
            self.0.rows(#storage).iter().map(|row| #row_name(row))
        }
    }
}

/// One child table's mutable-rows accessor.
fn rows_mut_tokens(t: &Table) -> TokenStream {
    let method = table_mut_ident(&t.ident);
    let storage = Literal::string(&t.name);
    let doc = docs(&format!(
        "The mutable rows of the {} child table (`{}`): append here, edit through the typed \
         row getters.",
        lower_first(&t.label),
        t.name
    ));
    quote! {
        #doc
        pub fn #method(&mut self) -> AppResult<&mut Vec<ChildRow>> {
            Ok(self.0.rows_mut(name_of(#storage)?))
        }
    }
}

/// One child table's typed row builder: appends an empty row and returns its typed view.
fn row_builder_tokens(t: &Table) -> TokenStream {
    let method = new_row_ident(&t.ident);
    let row_name = format_ident!("{}", t.row_name);
    let storage = Literal::string(&t.name);
    let doc = docs(&format!(
        "Appends one new, empty row to the {} child table (`{}`) and returns it, for the typed \
         row setters.",
        lower_first(&t.label),
        t.name
    ));
    quote! {
        #doc
        pub fn #method(&mut self) -> AppResult<#row_name<'_>> {
            let pushed = generated_push_row(&mut self.0, #storage, BTreeMap::new())?;
            Ok(#row_name(pushed))
        }
    }
}

/// The row view's struct.
fn row_type_tokens(row: &RowType) -> TokenStream {
    let name = format_ident!("{}", row.row_name);
    let doc = docs(&row.doc_comment());
    quote! {
        #doc
        pub struct #name<'a>(&'a ChildRow);
    }
}

/// The row view's impl: the typed getters over the row's own fields.
fn row_impl_tokens(row: &RowType) -> TokenStream {
    let name = format_ident!("{}", row.row_name);
    let mut methods = Vec::new();
    for f in &row.fields {
        methods.push(row_getter_tokens(f));
    }
    quote! {
        impl<'a> #name<'a> {
            /// The row's record id.
            #[must_use]
            pub fn id(&self) -> RecordId {
                self.0.id
            }

            #( #methods )*
        }
    }
}

/// A row view getter: reads the row's own fields, `Null` to `None`, else typed.
fn row_getter_tokens(s: &Scalar) -> TokenStream {
    let method = method(&s.ident);
    let storage = Literal::string(&s.name);
    let doc = docs(&s.doc_comment());
    let ty = getter_type(s.kind);
    let lookup = quote! { row_value_of(self.0, #storage) };
    match s.kind {
        Kind::Select => {
            let enum_name = s
                .select_of()
                .map_or_else(String::new, |e| e.enum_name.clone());
            let enum_ident = format_ident!("{enum_name}");
            let wrong = wrong_type_tokens(&s.name);
            quote! {
                #doc
                pub fn #method(&self) -> AppResult<Option<#enum_ident>> {
                    match #lookup {
                        Value::Null => Ok(None),
                        Value::Text(t) => #enum_ident::parse(t).map(Some),
                        _ => Err(#wrong),
                    }
                }
            }
        }
        Kind::Decimal | Kind::Link | Kind::DynamicLink | Kind::Text | Kind::Json => {
            // `Document` has the typed accessor; the row's fields are the same `Value` map.
            let arm = match s.kind {
                Kind::Decimal => quote! { Value::Decimal(d) => Ok(Some(*d)), },
                Kind::Link | Kind::DynamicLink => quote! { Value::Id(id) => Ok(Some(*id)), },
                _ => quote! { Value::Text(t) => Ok(Some(t.as_str())), },
            };
            let wrong = wrong_type_tokens(&s.name);
            quote! {
                #doc
                pub fn #method(&self) -> AppResult<Option<#ty>> {
                    match #lookup {
                        Value::Null => Ok(None),
                        #arm
                        _ => Err(#wrong),
                    }
                }
            }
        }
        _ => {
            let helper = generated_accessor(s.kind);
            quote! {
                #doc
                pub fn #method(&self) -> AppResult<Option<#ty>> {
                    #helper(#lookup, #storage)
                }
            }
        }
    }
}

/// The wrong-type error tokens, spelling the field.
fn wrong_type_tokens(storage: &str) -> TokenStream {
    let literal = Literal::string(storage);
    quote! {
        wrong_type(#literal)
    }
}

/// A select field's enum: `as_str` is the stored spelling; `parse` never panics.
fn select_tokens(select: &Select) -> TokenStream {
    let enum_name = format_ident!("{}", select.enum_name);
    let options = select
        .variants
        .iter()
        .map(|(text, _)| text.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let doc = docs(&format!("The options of the select field: {options}."));
    let variants = select
        .variants
        .iter()
        .map(|(text, variant)| {
            let variant = format_ident!("{variant}");
            let doc = docs(text);
            quote! {
                #doc
                #variant,
            }
        })
        .collect::<Vec<_>>();
    let as_str_arms = select.variants.iter().map(|(text, variant)| {
        let (text, variant) = (Literal::string(text), format_ident!("{variant}"));
        quote! { Self::#variant => #text, }
    });
    let parse_arms = select.variants.iter().map(|(text, variant)| {
        let (text, variant) = (Literal::string(text), format_ident!("{variant}"));
        quote! { #text => Ok(Self::#variant), }
    });
    quote! {
        #doc
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum #enum_name {
            #( #variants )*
        }

        impl #enum_name {
            /// The stored text of the option.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    #( #as_str_arms )*
                }
            }

            /// The value's stored form: `as_str` is the storage spelling.
            #[must_use]
            pub fn as_storage(self) -> &'static str {
                self.as_str()
            }

            /// Parses one stored option. An unknown value is an `AppError`, never a panic:
            /// the field's options may have been narrowed since the views were built.
            pub fn parse(text: &str) -> AppResult<Self> {
                match text {
                    #( #parse_arms )*
                    _ => Err(AppError::new(
                        ErrorClass::Internal,
                        "generated.document.wrong_select_value",
                        format!("`{text}` is not one of the field's options"),
                    )),
                }
            }
        }
    }
}

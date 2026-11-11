//! `#[controller]` and `#[whitelist]` for native controllers (ADR-16, WP-DOC-11.T1).
//!
//! A controller author writes ordinary async methods and marks the ones the API may call:
//!
//! ```ignore
//! #[controller(doctype = "time_entry")]
//! impl TimeEntryController {
//!     #[whitelist(right = "write")]
//!     async fn approve(&self, ctx: &mut dyn MethodContext, args: ApproveArgs) -> AppResult<Approved> {
//!         /* ctx.doc() is the loaded, row-locked document */
//!     }
//! }
//!
//! impl Controller for TimeEntryController {
//!     /* doctype, steps, run_step ... */
//!     fn methods(&self) -> Vec<MethodDecl> { Self::whitelisted_methods() }
//!     fn call_method<'a>(&'a self, method: &'a str, args: Json, ctx: &'a mut dyn MethodContext)
//!         -> BoxFuture<'a, AppResult<Json>> { self.whitelisted_call(method, args, ctx) }
//! }
//! ```
//!
//! A proc-macro crate can only export macros, and the app-facing `Controller` trait is a
//! contract this crate may not change, so `#[controller]` generates **inherent** items on the
//! impl block and the two `Controller` methods delegate to them (see [`controller`]).
//!
//! # What the expansion refers to
//!
//! Only `::spot_app_api` (G4: an app crate depends on nothing else of ours) and `::serde_json`,
//! which `Controller::call_method` already puts in the app's signatures. The macro never names
//! `serde`: the argument and result types need `Deserialize` / `Serialize`, and
//! `serde_json::from_value` / `to_value` enforce that with the error pointing at the type.

mod controller;
mod derive;
mod names;
mod schema;
mod serde_attrs;

use proc_macro::TokenStream;

/// Collects the `#[whitelist]` methods of an inherent impl block and generates, on the type:
///
/// * `WHITELIST_DOCTYPE: &'static str`, the `doctype = "..."` argument;
/// * `whitelisted_methods() -> Vec<MethodDecl>`, for `Controller::methods`;
/// * `whitelisted_call(&self, method, args, ctx) -> BoxFuture<AppResult<Json>>`, for
///   `Controller::call_method`: deserialises the arguments (unknown fields denied unless the
///   method says `allow_unknown`), calls the method, serialises its result, turns a serde
///   failure into `whitelist.bad_arguments` with the field path, and a panic into
///   `whitelist.failed`; a name that is not whitelisted is `method.not_found`;
/// * `whitelisted_rights() -> &'static [(&'static str, &'static str)]`, each method with the
///   right (or app permission) it needs;
/// * `whitelisted_schemas() -> serde_json::Value`, each method's right, `mutates` flag and
///   JSON Schema for its arguments and result, for the API's schema endpoint and the typed
///   client generator.
///
/// A method is `async fn name(&self, ctx: &mut dyn MethodContext[, args: Args]) -> AppResult<Out>`.
/// `#[whitelist(right = "write")]` takes `read`, `write` (the default), `submit`, `cancel` or
/// `create`; `permission = "approve_discount"` names an app-registered permission instead;
/// `name = "..."` renames the method on the wire; `mutates = false` overrides the default
/// (only `read` is read-only); `allow_unknown` lets the arguments carry fields the type does
/// not declare.
///
/// `Args` and `Out` need serde and the schema helper: `#[derive(MethodSchema)]` for your own
/// structs and unit-variant enums; primitives, `Option`, `Vec`, `Box`, string-keyed maps,
/// `Decimal`, the date and time types and `serde_json::Value` are built in.
#[proc_macro_attribute]
pub fn controller(attr: TokenStream, item: TokenStream) -> TokenStream {
    controller::expand(attr.into(), item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Marks a method of a `#[controller]` impl block as callable through the API. Only the
/// `#[controller]` macro reads it; anywhere else it is an error.
#[proc_macro_attribute]
pub fn whitelist(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item: proc_macro2::TokenStream = item.into();
    syn::Error::new_spanned(
        item,
        "#[whitelist] only works on a method inside an impl block marked #[controller(doctype = \"...\")]",
    )
    .to_compile_error()
    .into()
}

/// Generates `fn method_schema() -> serde_json::Value`, the JSON Schema of a struct with named
/// fields (or an enum of unit variants) made of primitives, options, vectors, maps and other
/// `MethodSchema` types. Honours `#[serde(rename = "...")]`, `default`, `skip` and
/// `skip_serializing_if`'s optionality; `rename_all`, `flatten`, `tag` and tuple or data
/// variants are refused with a clear message, because the schema could not match what serde
/// reads. The generated schema denies unknown properties; add `#[method_schema(allow_unknown)]`
/// to a struct to allow them.
#[proc_macro_derive(MethodSchema, attributes(method_schema, serde))]
pub fn derive_method_schema(input: TokenStream) -> TokenStream {
    derive::expand(input.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

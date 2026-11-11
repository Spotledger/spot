//! `#[controller(doctype = "...")]` on an inherent impl block.

use std::collections::BTreeSet;

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{
    Attribute, Error, FnArg, ImplItem, ImplItemFn, ItemImpl, Meta, Pat, PathArguments, ReturnType,
    Type, TypeParamBound,
};

use crate::names::check_name;
use crate::schema::schema_expr;

/// The rights a `#[whitelist(right = "...")]` may name.
const RIGHTS: &[&str] = &["read", "write", "submit", "cancel", "create"];

struct ControllerArgs {
    doctype: String,
}

/// One whitelisted method, validated.
struct Method {
    ident: syn::Ident,
    /// The name on the wire.
    name: String,
    /// The right, or the app permission when `custom`.
    right: String,
    custom: bool,
    mutates: bool,
    allow_unknown: bool,
    /// The type of the `args` parameter, if the method has one.
    args: Option<Type>,
    /// The `Out` of `AppResult<Out>`.
    out: Type,
}

/// Expands `#[controller]`.
pub fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let args = parse_args(attr)?;
    let mut imp: ItemImpl = syn::parse2(item)?;
    if let Some((_, path, _)) = &imp.trait_ {
        return Err(Error::new_spanned(
            path,
            "#[controller] goes on the type's own `impl` block (`impl MyController { .. }`), \
             not on a trait impl; delegate `Controller::methods` and `call_method` to the \
             generated `whitelisted_methods` and `whitelisted_call`",
        ));
    }
    if !imp.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &imp.generics,
            "#[controller] does not support generic controllers",
        ));
    }

    let mut methods: Vec<Method> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for item in &mut imp.items {
        let ImplItem::Fn(f) = item else {
            if let Some(attr) = item_attrs(item)
                .iter()
                .find(|a| a.path().is_ident("whitelist"))
            {
                return Err(Error::new_spanned(
                    attr,
                    "#[whitelist] only works on methods",
                ));
            }
            continue;
        };
        let Some(pos) = f.attrs.iter().position(|a| a.path().is_ident("whitelist")) else {
            continue;
        };
        let attr = f.attrs.remove(pos);
        if let Some(second) = f.attrs.iter().find(|a| a.path().is_ident("whitelist")) {
            return Err(Error::new_spanned(
                second,
                "a method has one #[whitelist] attribute",
            ));
        }
        let method = analyze(f, &attr)?;
        if !seen.insert(method.name.clone()) {
            return Err(Error::new(
                method.ident.span(),
                format!(
                    "the method name `{}` is whitelisted twice on this controller; whitelisted \
                     names are unique per DocType (rename one with #[whitelist(name = \"...\")])",
                    method.name
                ),
            ));
        }
        methods.push(method);
    }

    let generated = generate(&imp, &args, &methods);
    Ok(quote! {
        #imp
        #generated
    })
}

fn item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(c) => &c.attrs,
        ImplItem::Type(t) => &t.attrs,
        ImplItem::Macro(m) => &m.attrs,
        _ => &[],
    }
}

fn parse_args(attr: TokenStream) -> syn::Result<ControllerArgs> {
    let mut doctype: Option<String> = None;
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("doctype") {
            let lit: syn::LitStr = meta.value()?.parse()?;
            check_name(&lit.value(), "DocType name", lit.span())?;
            doctype = Some(lit.value());
            Ok(())
        } else {
            Err(meta.error("the only argument is `doctype = \"name\"`"))
        }
    });
    syn::parse::Parser::parse2(parser, attr)?;
    let doctype = doctype.ok_or_else(|| {
        Error::new(
            Span::call_site(),
            "#[controller] needs the DocType it serves: #[controller(doctype = \"time_entry\")]",
        )
    })?;
    Ok(ControllerArgs { doctype })
}

fn analyze(f: &ImplItemFn, attr: &Attribute) -> syn::Result<Method> {
    let sig = &f.sig;
    let mut name = sig.ident.to_string();
    name = name.strip_prefix("r#").map_or(name.clone(), str::to_owned);
    let mut name_span = sig.ident.span();
    let mut right = "write".to_owned();
    let mut custom = false;
    let mut mutates: Option<bool> = None;
    let mut allow_unknown = false;

    match &attr.meta {
        Meta::Path(_) => {}
        Meta::List(_) => attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("right") {
                let lit: syn::LitStr = meta.value()?.parse()?;
                if !RIGHTS.contains(&lit.value().as_str()) {
                    return Err(Error::new(
                        lit.span(),
                        format!(
                            "unknown right `{}`: expected one of {} (name an app-registered \
                             permission with `permission = \"...\"` instead)",
                            lit.value(),
                            RIGHTS.join(", ")
                        ),
                    ));
                }
                right = lit.value();
                custom = false;
            } else if meta.path.is_ident("permission") {
                let lit: syn::LitStr = meta.value()?.parse()?;
                check_name(&lit.value(), "permission name", lit.span())?;
                right = lit.value();
                custom = true;
            } else if meta.path.is_ident("name") {
                let lit: syn::LitStr = meta.value()?.parse()?;
                name = lit.value();
                name_span = lit.span();
            } else if meta.path.is_ident("mutates") {
                let lit: syn::LitBool = meta.value()?.parse()?;
                mutates = Some(lit.value);
            } else if meta.path.is_ident("allow_unknown") {
                allow_unknown = true;
            } else {
                return Err(meta.error(
                    "unknown option: use right, permission, name, mutates or allow_unknown",
                ));
            }
            Ok(())
        })?,
        Meta::NameValue(nv) => {
            return Err(Error::new_spanned(
                nv,
                "write #[whitelist] or #[whitelist(right = \"write\")]",
            ));
        }
    }
    check_name(&name, "method name", name_span)?;
    let mutates = mutates.unwrap_or(custom || right != "read");

    if sig.asyncness.is_none() {
        return Err(Error::new_spanned(
            sig.fn_token,
            "a whitelisted method must be `async fn`",
        ));
    }
    if !sig.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &sig.generics,
            "a whitelisted method cannot be generic",
        ));
    }
    let mut inputs = sig.inputs.iter();
    match inputs.next() {
        Some(FnArg::Receiver(r)) if r.reference.is_some() && r.mutability.is_none() => {}
        Some(first) => {
            return Err(Error::new_spanned(
                first,
                "a whitelisted method takes `&self` (a controller is shared between requests; \
                 keep its state behind the context or interior mutability)",
            ));
        }
        None => {
            return Err(Error::new_spanned(
                sig,
                "a whitelisted method takes `&self`, then `ctx: &mut dyn MethodContext`",
            ));
        }
    }
    match inputs.next() {
        Some(FnArg::Typed(t)) if is_method_context(&t.ty) => {}
        Some(other) => {
            return Err(Error::new_spanned(
                other,
                "the first parameter after `&self` must be `ctx: &mut dyn MethodContext`",
            ));
        }
        None => {
            return Err(Error::new_spanned(
                &sig.inputs,
                "a whitelisted method takes `ctx: &mut dyn MethodContext` after `&self`",
            ));
        }
    }
    let args = match inputs.next() {
        None => None,
        Some(FnArg::Typed(t)) => {
            if !matches!(*t.pat, Pat::Ident(_) | Pat::Wild(_)) {
                return Err(Error::new_spanned(
                    &t.pat,
                    "name the arguments parameter (`args: MyArgs`)",
                ));
            }
            Some((*t.ty).clone())
        }
        Some(other) => return Err(Error::new_spanned(other, "unexpected receiver")),
    };
    if let Some(extra) = inputs.next() {
        return Err(Error::new_spanned(
            extra,
            "a whitelisted method takes at most one arguments parameter; put the arguments in \
             one struct",
        ));
    }
    let out = result_ok_type(&sig.output)?;

    Ok(Method {
        ident: sig.ident.clone(),
        name,
        right,
        custom,
        mutates,
        allow_unknown,
        args,
        out,
    })
}

/// `&mut dyn MethodContext` (any path ending in `MethodContext`).
fn is_method_context(ty: &Type) -> bool {
    let Type::Reference(r) = ty else {
        return false;
    };
    if r.mutability.is_none() {
        return false;
    }
    let Type::TraitObject(t) = &*r.elem else {
        return false;
    };
    t.bounds.iter().any(|b| match b {
        TypeParamBound::Trait(tb) => tb
            .path
            .segments
            .last()
            .is_some_and(|s| s.ident == "MethodContext"),
        _ => false,
    })
}

/// The `Out` of `AppResult<Out>` / `Result<Out, AppError>`.
fn result_ok_type(output: &ReturnType) -> syn::Result<Type> {
    let ReturnType::Type(_, ty) = output else {
        return Err(Error::new_spanned(
            output,
            "a whitelisted method returns `AppResult<Out>`",
        ));
    };
    if let Type::Path(p) = &**ty
        && let Some(seg) = p.path.segments.last()
        && (seg.ident == "AppResult" || seg.ident == "Result")
        && let PathArguments::AngleBracketed(a) = &seg.arguments
        && let Some(syn::GenericArgument::Type(out)) = a.args.first()
    {
        return Ok(out.clone());
    }
    Err(Error::new_spanned(
        ty,
        "a whitelisted method returns `AppResult<Out>` (or `Result<Out, AppError>`)",
    ))
}

fn generate(imp: &ItemImpl, args: &ControllerArgs, methods: &[Method]) -> TokenStream {
    let self_ty = &imp.self_ty;
    let doctype = &args.doctype;

    let decls = methods.iter().map(|m| {
        let name = &m.name;
        let mutates = m.mutates;
        let right = &m.right;
        // An app permission whose name does not parse leaves the method out of the list, so it
        // is `method.not_found`: never a method that runs with a weaker right (fail closed).
        let named = |variant: &str| {
            let variant = syn::Ident::new(variant, Span::call_site());
            quote!(::std::result::Result::<_, ()>::Ok(::spot_app_api::MethodRight::#variant))
        };
        let right_expr = if m.custom {
            quote!(::spot_app_api::prelude::Name::new(#right)
                .map(::spot_app_api::MethodRight::Permission))
        } else {
            match right.as_str() {
                "read" => named("Read"),
                "write" => named("Write"),
                "create" => named("Create"),
                "submit" => named("Submit"),
                "cancel" => named("Cancel"),
                other => {
                    let message = format!("internal: unhandled right `{other}`");
                    quote!(compile_error!(#message))
                }
            }
        };
        quote! {
            if let (Ok(name), Ok(right)) = (::spot_app_api::prelude::Name::new(#name), #right_expr) {
                out.push(::spot_app_api::MethodDecl::new(name, #mutates).with_right(right));
            }
        }
    });
    let rights = methods.iter().map(|m| {
        let (name, right) = (&m.name, &m.right);
        quote!((#name, #right))
    });
    let schemas = methods.iter().map(|m| {
        let (name, right, custom, mutates) = (&m.name, &m.right, m.custom, m.mutates);
        let args_schema = m.args.as_ref().map_or_else(
            || quote!(::serde_json::json!({ "type": "null" })),
            schema_expr,
        );
        let out_schema = schema_expr(&m.out);
        quote! {
            schemas.insert(#name.to_owned(), ::serde_json::json!({
                "right": #right,
                "custom_permission": #custom,
                "mutates": #mutates,
                "arguments": #args_schema,
                "result": #out_schema,
            }));
        }
    });
    let arms = methods.iter().map(|m| {
        let (name, ident) = (&m.name, &m.ident);
        let parse = match &m.args {
            Some(ty) => {
                let check = if m.allow_unknown {
                    quote!()
                } else {
                    let schema = schema_expr(ty);
                    quote! {
                        let schema = #schema;
                        unknown_fields(method, &schema, &args)?;
                    }
                };
                quote! {
                    #check
                    let parsed: #ty = match ::serde_json::from_value::<#ty>(args) {
                        Ok(v) => v,
                        Err(e) => return Err(bad_arguments(method, &e.to_string(), None)),
                    };
                }
            }
            None => quote!(no_arguments(method, &args)?;),
        };
        let call = if m.args.is_some() {
            quote!(self.#ident(ctx, parsed).await?)
        } else {
            quote!(self.#ident(ctx).await?)
        };
        quote! {
            #name => guarded(method, ::std::boxed::Box::pin(async move {
                #parse
                let out = #call;
                ::serde_json::to_value(&out).map_err(|e| ::spot_app_api::AppError::new(
                    ::spot_app_api::ErrorClass::Internal,
                    "whitelist.failed",
                    format!("the result of `{method}` cannot be written as JSON: {e}"),
                ))
            })),
        }
    });

    quote! {
        impl #self_ty {
            /// The DocType this controller serves.
            pub const WHITELIST_DOCTYPE: &'static str = #doctype;

            /// The whitelisted methods, for `Controller::methods`.
            #[must_use]
            pub fn whitelisted_methods() -> ::std::vec::Vec<::spot_app_api::MethodDecl> {
                let mut out = ::std::vec::Vec::new();
                #(#decls)*
                out
            }

            /// Each whitelisted method with the right (or app permission) it needs.
            #[must_use]
            pub fn whitelisted_rights() -> &'static [(&'static str, &'static str)] {
                &[#(#rights),*]
            }

            /// Each whitelisted method's right, `mutates` flag and JSON Schema for its
            /// arguments and result, keyed by method name.
            #[must_use]
            pub fn whitelisted_schemas() -> ::serde_json::Value {
                #[allow(unused_mut)]
                let mut schemas = ::serde_json::Map::new();
                #(#schemas)*
                ::serde_json::Value::Object(schemas)
            }

            /// Runs the whitelisted method `method`, for `Controller::call_method`. A name that
            /// is not whitelisted is `method.not_found`.
            pub fn whitelisted_call<'a>(
                &'a self,
                method: &'a str,
                args: ::serde_json::Value,
                ctx: &'a mut dyn ::spot_app_api::MethodContext,
            ) -> ::spot_app_api::BoxFuture<'a, ::spot_app_api::AppResult<::serde_json::Value>> {
                #[allow(dead_code)]
                fn bad_arguments(
                    method: &str,
                    detail: &str,
                    field: ::std::option::Option<&str>,
                ) -> ::spot_app_api::AppError {
                    let message = format!("the arguments of `{method}` are not valid: {detail}");
                    // serde names the field only inside the message of a missing, unknown or
                    // duplicate field.
                    let named = field.or_else(|| {
                        let named = detail.starts_with("missing field")
                            || detail.starts_with("unknown field")
                            || detail.starts_with("duplicate field");
                        let start = detail.find('`')? + 1;
                        let end = start + detail[start..].find('`')?;
                        named.then(|| &detail[start..end])
                    });
                    match named.and_then(::spot_app_api::FieldPath::parse) {
                        ::std::option::Option::Some(path) => ::spot_app_api::AppError::field(
                            path,
                            "whitelist.bad_arguments",
                            message,
                        ),
                        ::std::option::Option::None => ::spot_app_api::AppError::new(
                            ::spot_app_api::ErrorClass::Validation,
                            "whitelist.bad_arguments",
                            message,
                        ),
                    }
                }

                #[allow(dead_code)]
                fn unknown_fields(
                    method: &str,
                    schema: &::serde_json::Value,
                    args: &::serde_json::Value,
                ) -> ::spot_app_api::AppResult<()> {
                    let (::std::option::Option::Some(props), ::std::option::Option::Some(given)) =
                        (schema.get("properties").and_then(::serde_json::Value::as_object), args.as_object())
                    else {
                        return ::std::result::Result::Ok(());
                    };
                    if schema.get("additionalProperties") != ::std::option::Option::Some(&::serde_json::Value::Bool(false)) {
                        return ::std::result::Result::Ok(());
                    }
                    for key in given.keys() {
                        if !props.contains_key(key) {
                            return ::std::result::Result::Err(bad_arguments(
                                method,
                                &format!("unknown field `{key}`"),
                                ::std::option::Option::Some(key),
                            ));
                        }
                    }
                    ::std::result::Result::Ok(())
                }

                #[allow(dead_code)]
                fn no_arguments(
                    method: &str,
                    args: &::serde_json::Value,
                ) -> ::spot_app_api::AppResult<()> {
                    let empty = args.is_null() || args.as_object().is_some_and(|o| o.is_empty());
                    if empty {
                        ::std::result::Result::Ok(())
                    } else {
                        ::std::result::Result::Err(bad_arguments(method, "this method takes no arguments", ::std::option::Option::None))
                    }
                }

                /// Turns a panic inside the method into `whitelist.failed`; the error aborts
                /// the method's transaction like any other.
                struct Guard<'g> {
                    method: &'g str,
                    fut: ::spot_app_api::BoxFuture<'g, ::spot_app_api::AppResult<::serde_json::Value>>,
                }

                impl ::std::future::Future for Guard<'_> {
                    type Output = ::spot_app_api::AppResult<::serde_json::Value>;

                    fn poll(
                        mut self: ::std::pin::Pin<&mut Self>,
                        cx: &mut ::std::task::Context<'_>,
                    ) -> ::std::task::Poll<Self::Output> {
                        let this = &mut *self;
                        let polled = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                            this.fut.as_mut().poll(cx)
                        }));
                        match polled {
                            ::std::result::Result::Ok(p) => p,
                            ::std::result::Result::Err(_) => ::std::task::Poll::Ready(
                                ::std::result::Result::Err(::spot_app_api::AppError::new(
                                    ::spot_app_api::ErrorClass::Internal,
                                    "whitelist.failed",
                                    format!(
                                        "the method `{}` failed unexpectedly; its changes were rolled back",
                                        this.method
                                    ),
                                )),
                            ),
                        }
                    }
                }

                #[allow(dead_code)]
                fn guarded<'g>(
                    method: &'g str,
                    fut: ::spot_app_api::BoxFuture<'g, ::spot_app_api::AppResult<::serde_json::Value>>,
                ) -> ::spot_app_api::BoxFuture<'g, ::spot_app_api::AppResult<::serde_json::Value>> {
                    ::std::boxed::Box::pin(Guard { method, fut })
                }

                #[allow(unused_variables)]
                let (args, ctx) = (args, ctx);
                match method {
                    #(#arms)*
                    _ => ::std::boxed::Box::pin(async move {
                        ::std::result::Result::Err(::spot_app_api::AppError::new(
                            ::spot_app_api::ErrorClass::MethodNotFound,
                            "method.not_found",
                            format!("no whitelisted method `{method}` on this controller"),
                        ))
                    }),
                }
            }
        }
    }
}

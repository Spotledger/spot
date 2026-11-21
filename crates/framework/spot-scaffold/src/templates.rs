//! The compiled-in template sources. Every template lives under `templates/` and is
//! `include_str!`ed here, so a template that rots is a build (or test) failure, never a
//! runtime surprise.
//!
//! The names carry no dot: the renderer picks the output kind from the name's extension, and
//! a dot in a name would read as a dotted error code to the error-code scan. Every generated
//! file is plain text, so the extensionless name is right.

/// Every template, by name, in a stable order. The names are the keys the renderer uses; the
/// output path is chosen by the generator, not by the template name.
pub const TEMPLATES: &[(&str, &str)] = &[
    ("app_cargo", include_str!("../templates/app_cargo.j2")),
    (
        "app_controllers",
        include_str!("../templates/app_controllers.j2"),
    ),
    ("app_doctype", include_str!("../templates/app_doctype.j2")),
    (
        "app_fixtures_readme",
        include_str!("../templates/app_fixtures_readme.j2"),
    ),
    (
        "app_gitignore",
        include_str!("../templates/app_gitignore.j2"),
    ),
    ("app_grants", include_str!("../templates/app_grants.j2")),
    ("app_lib", include_str!("../templates/app_lib.j2")),
    ("app_manifest", include_str!("../templates/app_manifest.j2")),
    ("app_readme", include_str!("../templates/app_readme.j2")),
    (
        "app_test_load",
        include_str!("../templates/app_test_load.j2"),
    ),
    (
        "controller_skeleton",
        include_str!("../templates/controller_skeleton.j2"),
    ),
    (
        "doctype_grant_row",
        include_str!("../templates/doctype_grant_row.j2"),
    ),
    ("doctype_test", include_str!("../templates/doctype_test.j2")),
    ("script_route", include_str!("../templates/script_route.j2")),
    ("script_types", include_str!("../templates/script_types.j2")),
];

/// The minimal WebAssembly component every generated app carries as `app.wasm`.
///
/// A bundle must hold a component (`spot-bundle`'s `Bundle::open` refuses one without), so a
/// scaffold writes this tiny pre-assembled component (it imports the host's `log`, so it needs
/// no declared capability). The binary is the same shape `spot-bundle`'s own tests use; a
/// generator that ships a native controller or a script still carries it.
pub const COMPONENT: &[u8] = include_bytes!("../templates/app_component.wasm");

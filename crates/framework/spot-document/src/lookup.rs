//! Finding a DocType's effective definition and the definitions of its child tables.

use spot_app_api::error::{AppError, ErrorClass};
use spot_app_api::prelude::Name;
use spot_meta::{Definition, EffectiveMeta, FieldSpec};

/// A DocType with the definitions of its child tables resolved.
pub(crate) struct Resolved<'a> {
    pub(crate) def: &'a Definition,
    /// `(table field, child definition)` in field order.
    pub(crate) tables: Vec<(&'a FieldSpec, &'a Definition)>,
}

impl<'a> Resolved<'a> {
    /// The table field called `name`.
    pub(crate) fn table(&self, name: &str) -> Option<(&'a FieldSpec, &'a Definition)> {
        self.tables.iter().find(|(f, _)| f.name == name).copied()
    }
}

/// The effective definition of `doctype` and its tables.
pub(crate) fn resolve<'a>(
    meta: &'a EffectiveMeta,
    doctype: &str,
) -> Result<Resolved<'a>, AppError> {
    let def = meta.definition(doctype).ok_or_else(|| {
        AppError::new(
            ErrorClass::UnknownDocType,
            "meta.unknown_doctype",
            format!("The DocType `{doctype}` does not exist."),
        )
    })?;
    let mut tables = Vec::new();
    for field in def.fields.iter().filter(|f| f.ty.is_table()) {
        let target = field.options.as_deref().unwrap_or("");
        let child = meta.definition(target).ok_or_else(|| {
            AppError::new(
                ErrorClass::Internal,
                "document.unknown_child_doctype",
                format!(
                    "The table `{doctype}.{}` uses the DocType `{target}`, which the metadata does not define.",
                    field.name
                ),
            )
        })?;
        tables.push((field, child));
    }
    Ok(Resolved { def, tables })
}

/// A field name as a storage [`Name`]. Metadata validates names, so a failure is an internal
/// error, not the caller's.
pub(crate) fn name(s: &str) -> Result<Name, AppError> {
    Name::new(s).map_err(|e| {
        AppError::new(
            ErrorClass::Internal,
            "document.invalid_name",
            format!("The metadata holds a name that storage refuses: {e}"),
        )
    })
}

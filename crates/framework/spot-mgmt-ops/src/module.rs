//! Registration by modules: the Linux, Windows and database modules each implement
//! [`OperationModule`] and the host assembles the catalogue from them.

/// One operation module: a component that owns a family of management operations and
/// registers them on the catalogue builder.
///
/// The Linux, Windows and database modules (WP-ADM-04 to ADM-06) each export one `register`
/// function; this trait is the shape the host calls and the tests emulate with toy modules.
pub trait OperationModule {
    /// The module's name, used in catalogue errors (a duplicate id names both modules).
    fn name(&self) -> &str;

    /// Registers the module's operations on `b`.
    fn register(&self, b: &mut crate::CatalogueBuilder);
}

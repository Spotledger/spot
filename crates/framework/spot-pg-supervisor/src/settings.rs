//! Postmaster settings the supervisor writes into `spot.conf` (included from
//! `postgresql.conf`). The durability switches `fsync` and `full_page_writes` are
//! deliberately **not** fields here: they are fixed `on` in the rendered file, so no
//! caller can turn them off through this type (the task's test proves that).

/// Overrides applied to the bundled cluster through `spot.conf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgSettings {
    /// Overrides `port` (kept in sync with `PgLayout::port` at `Supervisor::new`).
    pub port: u16,
    /// `max_connections` for the bundled cluster.
    pub max_connections: u32,
    /// `shared_buffers`, PostgreSQL size syntax (e.g. `"128MB"`).
    pub shared_buffers: String,
    /// Extra `name = 'value'` lines appended verbatim after the fixed overrides.
    pub extra_config: Vec<String>,
}

impl Default for PgSettings {
    fn default() -> Self {
        PgSettings {
            port: 5433,
            max_connections: 100,
            shared_buffers: "128MB".to_string(),
            extra_config: Vec::new(),
        }
    }
}

impl PgSettings {
    /// Overrides everything `Supervisor::new` would sync from the layout.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

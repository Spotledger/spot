//! Shared helpers: a scratch database and a small catalogue builder.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_pg_migrate::{ApplyOptions, LiveSchema, MigrationPlan, apply, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Catalog, ChildTableDef, Collation, EntityDef, EntityKind, FieldDef, FieldType, IndexDef,
    LinkCheck, LinkTarget, Name,
};

pub fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

pub fn field(name: &str, ty: FieldType, nullable: bool) -> FieldDef {
    FieldDef {
        name: n(name),
        ty,
        nullable,
    }
}

pub fn text(max: Option<u32>) -> FieldType {
    FieldType::Text { max_chars: max }
}

pub fn dec(precision: u8, scale: u8) -> FieldType {
    FieldType::Decimal { precision, scale }
}

pub fn link(target: &str) -> FieldType {
    FieldType::Link {
        target: LinkTarget::Entity(n(target)),
        check: LinkCheck::Database,
    }
}

pub fn doc(name: &str, fields: Vec<FieldDef>) -> EntityDef {
    EntityDef {
        name: n(name),
        kind: EntityKind::Document,
        fields,
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    }
}

pub fn child(name: &str, parent: &str, fields: Vec<FieldDef>) -> EntityDef {
    EntityDef {
        name: n(name),
        kind: EntityKind::Child { parent: n(parent) },
        fields,
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    }
}

pub fn with_child(mut e: EntityDef, field: &str, entity: &str) -> EntityDef {
    e.children.push(ChildTableDef {
        field: n(field),
        entity: n(entity),
    });
    e
}

pub fn with_index(mut e: EntityDef, name: &str, fields: &[&str], unique: bool) -> EntityDef {
    e.indexes.push(IndexDef {
        name: n(name),
        fields: fields.iter().map(|f| n(f)).collect(),
        unique,
        method: spot_storage_contract::IndexMethod::BTree,
        opclass: None,
        include: vec![],
    });
    e
}

pub fn catalog(version: u64, entities: Vec<EntityDef>) -> Catalog {
    Catalog::new(version, Collation::new("en").unwrap(), entities).unwrap()
}

pub struct Rig {
    pub db: TestDb,
    pub backend: PgBackend,
}

/// A scratch database, or `None` (and a SKIPPED line) when no server is configured.
pub async fn rig() -> Option<Rig> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    Some(Rig { db, backend })
}

impl Rig {
    pub async fn live(&self) -> LiveSchema {
        LiveSchema::read(&self.backend).await.unwrap()
    }

    pub async fn plan(&self, wanted: &Catalog) -> MigrationPlan {
        plan(&self.live().await, wanted)
    }

    pub async fn plan_with_defaults(
        &self,
        wanted: &Catalog,
        options: &spot_pg_migrate::PlanOptions,
    ) -> MigrationPlan {
        spot_pg_migrate::plan_with(&self.live().await, wanted, options)
    }

    pub async fn migrate(&self, wanted: &Catalog) -> spot_pg_migrate::ApplyReport {
        let p = self.plan(wanted).await;
        apply(&self.backend, &p, ApplyOptions::default())
            .await
            .unwrap()
    }
}

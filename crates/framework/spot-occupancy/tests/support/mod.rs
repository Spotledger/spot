//! Shared setup: a scratch PostgreSQL database with `btree_gist`, the occupancy catalog, units
//! and services on a manual clock. Every test that needs the database skips (printing
//! `SKIPPED:`) when `SPOT_TEST_PG_URL` is unset; `SPOT_REQUIRE_PG=1` turns that into a failure.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use spot_core::{Clock, ManualClock};
use spot_meta::{Customisations, Definition, FieldSpec, FieldType, Registry};
use spot_occupancy::{
    ClaimRequest, Holder, HubOwnsAll, Limits, MemoryAudit, Occupancy, Owner, OwnerRoutes, Span,
    definition,
};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Catalog, ChangeFeed, Collation, Document, DocumentData, DocumentStore, FeedPosition, Fields,
    Isolation, Name, RangePoint, RangeValue, RecordId, RowLock, StorageBackend, Transaction,
    TxOptions, Value,
};
use time::{Date, Month, OffsetDateTime};

pub type Svc = Occupancy<PgBackend>;

pub fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

pub struct Fx {
    pub db: TestDb,
    pub backend: Arc<PgBackend>,
    pub catalog: Arc<Catalog>,
    pub clock: Arc<ManualClock>,
    pub audit: Arc<MemoryAudit>,
    pub svc: Arc<Svc>,
    pub units: Vec<RecordId>,
}

pub fn start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

/// A scratch database with the occupancy catalog prepared and `units` rooms, or `None`.
pub async fn fixture(units: usize, span: Span, limits: Limits) -> Option<Fx> {
    let db = TestDb::create().await?;
    let missing = db.create_extensions(&["btree_gist"]).await;
    if !missing.is_empty() {
        assert!(
            std::env::var("SPOT_REQUIRE_PG").as_deref() != Ok("1"),
            "SPOT_REQUIRE_PG=1 but the server does not offer {missing:?}"
        );
        println!("SKIPPED: the server does not offer {missing:?}");
        return None;
    }
    let mut room = Definition::new("room");
    room.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let meta = Registry::build(vec![room, definition("room", span)], &Customisations::new())
        .unwrap_or_else(|e| panic!("the occupancy definition does not build: {e:?}"));
    let catalog = Arc::new(meta.catalog(Collation::new("und").unwrap()).unwrap());
    let backend = Arc::new(PgBackend::connect(db.config()).await.expect("connect"));
    backend.prepare(&catalog).await.expect("prepare");

    let mut ids = Vec::new();
    let mut tx = backend
        .begin(
            catalog.clone(),
            TxOptions::new(n("test_units"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    for i in 0..units {
        let id = RecordId::new_v7();
        let mut fields = Fields::new();
        fields.insert(n("name"), Value::Text(format!("room {i}")));
        tx.insert(&DocumentData {
            entity: n("room"),
            id,
            fields,
            children: BTreeMap::new(),
        })
        .await
        .unwrap();
        ids.push(id);
    }
    tx.commit().await.unwrap();

    let clock = Arc::new(ManualClock::new(start()));
    let audit = Arc::new(MemoryAudit::new());
    let svc = Arc::new(
        Occupancy::new(
            backend.clone(),
            catalog.clone(),
            clock.clone() as Arc<dyn Clock>,
            Owner::Hub,
            Arc::new(HubOwnsAll),
            audit.clone(),
            limits,
        )
        .expect("the service builds"),
    );
    Some(Fx {
        db,
        backend,
        catalog,
        clock,
        audit,
        svc,
        units: ids,
    })
}

impl Fx {
    /// Another service on the same database with its own clock, node and routes.
    pub fn service(
        &self,
        clock: Arc<ManualClock>,
        node: Owner,
        routes: Arc<dyn OwnerRoutes>,
        limits: Limits,
    ) -> Arc<Svc> {
        Arc::new(
            Occupancy::new(
                self.backend.clone(),
                self.catalog.clone(),
                clock as Arc<dyn Clock>,
                node,
                routes,
                self.audit.clone(),
                limits,
            )
            .expect("the service builds"),
        )
    }

    /// `(topic, subject id)` of every published outbox event, in position order.
    pub async fn topics(&self) -> Vec<(String, RecordId)> {
        self.backend
            .read_outbox(FeedPosition::START, 1_000_000)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.event.topic.as_str().to_owned(), r.event.subject.id))
            .collect()
    }

    pub async fn count(&self, topic: &str) -> usize {
        self.topics()
            .await
            .iter()
            .filter(|(t, _)| t == topic)
            .count()
    }

    /// The stored record, or `None`.
    pub async fn load(&self, id: RecordId) -> Option<Document> {
        let mut tx = self
            .backend
            .begin(
                self.catalog.clone(),
                TxOptions::new(n("test_read"), Isolation::ReadCommitted),
            )
            .await
            .unwrap();
        let d = tx.load(&n("occupancy"), id, RowLock::None).await.unwrap();
        tx.rollback().await.unwrap();
        d
    }

    /// Writes an occupancy row directly, bypassing the service (a restore, a test setup).
    pub fn raw(
        &self,
        unit: RecordId,
        during: RangeValue,
        state: &str,
        holder: &str,
        expires_at: Option<OffsetDateTime>,
    ) -> DocumentData {
        let mut fields = Fields::new();
        fields.insert(n("unit"), Value::Id(unit));
        fields.insert(n("during"), Value::Range(during));
        fields.insert(n("state"), Value::Text(state.to_owned()));
        fields.insert(n("holder"), Value::Text(holder.to_owned()));
        fields.insert(
            n("expires_at"),
            expires_at.map_or(Value::Null, Value::DateTime),
        );
        DocumentData {
            entity: n("occupancy"),
            id: RecordId::new_v7(),
            fields,
            children: BTreeMap::new(),
        }
    }

    pub async fn begin(&self, label: &str) -> <PgBackend as StorageBackend>::Tx {
        self.backend
            .begin(
                self.catalog.clone(),
                TxOptions::new(n(label), Isolation::ReadCommitted),
            )
            .await
            .unwrap()
    }
}

pub fn day(d: i64) -> Date {
    Date::from_calendar_date(2026, Month::November, 1).unwrap() + time::Duration::days(d - 1)
}

/// The nights `[from, to)` of November 2026 (dates).
pub fn nights(from: i64, to: i64) -> RangeValue {
    RangeValue::half_open(RangePoint::Date(day(from)), RangePoint::Date(day(to)))
}

pub fn holder(id: &str) -> Holder {
    Holder::parse(&format!("user:{id}")).unwrap()
}

pub fn claim(unit: RecordId, from: i64, to: i64, who: &str) -> ClaimRequest {
    ClaimRequest {
        unit,
        during: nights(from, to),
        holder: holder(who),
        ttl: None,
    }
}

pub fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

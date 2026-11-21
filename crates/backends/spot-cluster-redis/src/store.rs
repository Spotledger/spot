//! The Redis-backed [`VersionStore`]: the site's current metadata version and content hash.
//!
//! One small hash holds the head, so a node that polls it (the [`PollingBus`](spot_meta_live::PollingBus)
//! pattern) and a node that subscribes to the bus agree on the same record. The store is a plain
//! key-value pair; it never decides whether a version is newer, the follower does.

use spot_meta_live::{LiveError, MetaHash, StoreFuture, Version, VersionStore};

use crate::connection::RedisConnection;

/// The head record of the site's metadata version, in Redis.
#[derive(Clone, Debug)]
pub struct RedisVersionStore {
    connection: RedisConnection,
}

impl RedisVersionStore {
    /// A version store over `connection`.
    #[must_use]
    pub fn new(connection: RedisConnection) -> Self {
        Self { connection }
    }

    /// The key holding the head.
    #[must_use]
    pub fn key(&self) -> String {
        self.connection.keyspace().meta_head()
    }
}

impl VersionStore for RedisVersionStore {
    fn read<'a>(&'a self) -> StoreFuture<'a, Option<(Version, MetaHash)>> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            let fields: Vec<Option<String>> = redis::cmd("HMGET")
                .arg(self.key())
                .arg("version")
                .arg("hash")
                .query_async(&mut manager)
                .await
                .map_err(|error| LiveError::Store {
                    message: error.to_string(),
                })?;
            let version = fields
                .first()
                .cloned()
                .flatten()
                .and_then(|text| text.parse::<u64>().ok());
            let hash = fields
                .get(1)
                .cloned()
                .flatten()
                .filter(|text| !text.is_empty());
            match (version, hash) {
                (Some(version), Some(hash)) => Ok(Some((
                    Version::new(version),
                    MetaHash::from_hex(hash.clone()),
                ))),
                _ => Ok(None),
            }
        })
    }

    fn write<'a>(&'a self, version: Version, hash: MetaHash) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut manager = self.connection.manager();
            redis::cmd("HSET")
                .arg(self.key())
                .arg("version")
                .arg(version.get().to_string())
                .arg("hash")
                .arg(hash.as_hex())
                .query_async::<i64>(&mut manager)
                .await
                .map_err(|error| LiveError::Store {
                    message: error.to_string(),
                })?;
            Ok(())
        })
    }
}

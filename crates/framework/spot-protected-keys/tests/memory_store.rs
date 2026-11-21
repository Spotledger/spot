//! The `KeyStore` contract suite on the in-memory store.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

#[path = "support/contract.rs"]
mod contract;

use std::sync::Arc;

use spot_protected_keys::{KeyStore, MemoryKeyStore, TenantId};

struct Memory {
    base: MemoryKeyStore,
}

impl contract::Harness for Memory {
    fn open(&self, tenant: &str) -> Arc<dyn KeyStore> {
        Arc::new(self.base.sibling(TenantId::new(tenant).unwrap()))
    }
}

#[test]
fn the_contract_suite_passes_on_memory() {
    let h = Memory {
        base: MemoryKeyStore::new(TenantId::new("base").unwrap()),
    };
    contract::run_all(&h);
}

#[test]
fn the_ports_codes_are_catalogued_and_stable() {
    use spot_protected_keys::store::{BAD_INPUT, CORRUPT, EPOCH_CONFLICT, STORE_FAILED};
    use spot_protected_keys::{GroupName, KeyStoreError, ProtectedKeyError};
    let group = GroupName::new("bank").unwrap();
    let probes = [
        (
            KeyStoreError::EpochConflict {
                group: group.clone(),
            },
            EPOCH_CONFLICT,
        ),
        (KeyStoreError::BadInput("x"), BAD_INPUT),
        (
            KeyStoreError::Failed("pg key store: x".to_owned()),
            STORE_FAILED,
        ),
        (KeyStoreError::Corrupt("x"), CORRUPT),
        (
            KeyStoreError::Key(ProtectedKeyError::UnknownGroup { group }),
            "protected_keys.unknown_group",
        ),
    ];
    let catalogue = spot_errors::Catalogue::load();
    assert_eq!(catalogue.load_error(), None);
    for (error, code) in probes {
        assert_eq!(error.code(), code);
        assert!(error.to_string().starts_with(code), "{error}");
        assert!(
            catalogue.lookup(code).is_some(),
            "no catalogue entry for `{code}`"
        );
    }
}

#[test]
fn the_memory_store_checks_its_inputs_like_the_database_store() {
    use spot_protected_keys::store::{MIN_WRAPPED_LEN, check_wrapped};
    assert!(check_wrapped(&[0; MIN_WRAPPED_LEN - 1]).is_err());
    assert!(check_wrapped(&[0; MIN_WRAPPED_LEN]).is_ok());
    assert!(check_wrapped(&[0; 4097]).is_err());
}

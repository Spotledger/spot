//! The salt provider of the usage emitter.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_cli::services::metering::{SiteSalt, member_digest};
use spot_cli::services::metering_ports::StoredSalt;

#[test]
fn a_short_or_zero_salt_is_refused_without_naming_it() {
    for bad in [vec![], vec![1_u8; 15], vec![0_u8; 32]] {
        let error = StoredSalt::new(bad).err().unwrap();
        assert!(error.contains("16 bytes"), "{error}");
    }
}

#[test]
fn a_salt_gives_stable_digests_per_site_and_never_prints() {
    let a = StoredSalt::new(vec![7_u8; 32]).unwrap();
    let b = StoredSalt::new(vec![8_u8; 32]).unwrap();
    assert_eq!(
        member_digest(&a.salt(), "u1"),
        member_digest(&a.salt(), "u1")
    );
    assert_ne!(
        member_digest(&a.salt(), "u1"),
        member_digest(&b.salt(), "u1")
    );
    assert_eq!(format!("{a:?}"), "StoredSalt(..)");
}

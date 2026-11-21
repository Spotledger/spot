//! The tail chunk codec, version 1 (read only) and the rules A.3 (a) to (e) that both versions share.

use crate::tail::{self, Rule, v1};

use super::support::{TENANT, input, line, pos, tail_key, tail_keys};

const V1_FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/tail/v1-gen1-batch1.chunk");

async fn read(bytes: &[u8]) -> Result<tail::TailChunk, tail::ChunkError> {
    tail::decode(TENANT, 1, Some(&tail_keys()), &tail_key(1, 1), bytes).await
}

fn rule_of(result: Result<tail::TailChunk, tail::ChunkError>) -> Rule {
    result.expect_err("refused").rule()
}

#[tokio::test]
async fn a_version_1_chunk_round_trips_through_the_reader() {
    let entries = input(1, 1, 5);
    let bytes = v1::encode(&entries).expect("encodes");
    let chunk = read(&bytes).await.expect("reads");
    assert_eq!(chunk.version, 1);
    assert_eq!(chunk.batch_id, 1);
    assert_eq!(chunk.entries, entries);
    assert_eq!((chunk.first_seq(), chunk.last_seq()), (1, 5));
}

#[tokio::test]
async fn the_version_1_fixture_decodes_to_its_expected_entries() {
    let chunk = read(V1_FIXTURE)
        .await
        .expect("the fixture is a valid v1 chunk");
    assert_eq!(chunk.version, 1);
    assert_eq!(chunk.entries.len(), 3);
    for (index, (position, text)) in chunk.entries.iter().enumerate() {
        let seq = u64::try_from(index).unwrap() + 1;
        assert_eq!(*position, pos(seq));
        assert_eq!(
            String::from_utf8(text.clone()).unwrap(),
            format!("{{\"g\":1,\"seq\":{seq},\"data\":\"entry number {seq}\"}}")
        );
    }
    // The bytes are the ones the merged relay wrote: the checksum of the proposal's worked example.
    assert_eq!(
        &V1_FIXTURE[V1_FIXTURE.len() - 32..][..4],
        &[0x66, 0x5b, 0x1a, 0x2e]
    );
    // And the reader needs no keys for it.
    assert!(
        tail::decode(TENANT, 1, None, &tail_key(1, 1), V1_FIXTURE)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_version_1_chunk_with_one_changed_byte_is_refused() {
    let bytes = v1::encode(&input(1, 1, 3)).expect("encodes");
    for index in 0..bytes.len() {
        let mut damaged = bytes.clone();
        damaged[index] ^= 0x01;
        assert!(read(&damaged).await.is_err(), "byte {index}");
    }
}

#[tokio::test]
async fn a_truncated_or_extended_chunk_is_refused() {
    let bytes = v1::encode(&input(1, 1, 3)).expect("encodes");
    for cut in [0, 1, 8, 31, 32, bytes.len() - 1] {
        assert_eq!(rule_of(read(&bytes[..cut]).await), Rule::B, "cut at {cut}");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(read(&longer).await.is_err());
}

#[tokio::test]
async fn a_zero_count_chunk_decodes_but_is_invalid() {
    // A.3 (d): the bytes are well formed (checksum and count agree) and the chunk is still refused.
    let bytes = v1::encode(&[]).expect("encodes");
    assert_eq!(rule_of(read(&bytes).await), Rule::D);
    let one = v1::encode(&[(pos(1), line(1, 1))]).expect("encodes");
    assert_eq!(read(&one).await.map(|chunk| chunk.entries.len()), Ok(1));
}

#[tokio::test]
async fn every_a3_rule_has_a_failing_case() {
    let good = v1::encode(&input(1, 1, 3)).unwrap();
    // (a) the key is not a canonical tail key of this tenant and generation.
    for key in [
        tail_key(2, 1),
        "acme/tail/1/1.chunk".to_owned(),
        "acme/segments/00000000000000000001/00000000000000000001-00000000000000000003.seg"
            .to_owned(),
        "garbage".to_owned(),
    ] {
        assert_eq!(
            tail::decode(TENANT, 1, None, &key, &good)
                .await
                .unwrap_err()
                .rule(),
            Rule::A,
            "{key}"
        );
    }
    assert_eq!(
        tail::decode("other", 1, None, &tail_key(1, 1), &good)
            .await
            .unwrap_err()
            .rule(),
        Rule::A
    );
    // (b) too short, or the last 32 bytes are not the checksum.
    assert_eq!(rule_of(read(&good[..31]).await), Rule::B);
    let mut bad_sum = good.clone();
    *bad_sum.last_mut().unwrap() ^= 1;
    assert_eq!(rule_of(read(&bad_sum).await), Rule::B);
    // (c) an unknown magic fails closed; entries that run past the end.
    let mut other_magic = good.clone();
    other_magic[7] = b'9';
    assert_eq!(rule_of(read(&other_magic).await), Rule::C);
    let mut over_count = v1::encode(&input(1, 1, 3)).unwrap();
    over_count[8..12].copy_from_slice(&4u32.to_be_bytes());
    let body_len = over_count.len() - 32;
    let resealed = reseal_v1(&over_count[..body_len]);
    assert_eq!(rule_of(read(&resealed).await), Rule::C);
    // (d) another generation in an entry.
    assert_eq!(
        rule_of(read(&v1::encode(&input(2, 1, 3)).unwrap()).await),
        Rule::D
    );
    // (e) a gap or a repeat inside the chunk.
    let mut gapped = input(1, 1, 3);
    gapped[2].0 = pos(5);
    assert_eq!(rule_of(read(&v1::encode(&gapped).unwrap()).await), Rule::E);
    let mut repeated = input(1, 1, 3);
    repeated[1].0 = pos(1);
    assert_eq!(
        rule_of(read(&v1::encode(&repeated).unwrap()).await),
        Rule::E
    );
}

/// A version 1 object with a valid checksum over `body`.
fn reseal_v1(body: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"spot.journal.tail.v1\0");
    hasher.update(body);
    let mut out = body.to_vec();
    out.extend_from_slice(&hasher.finalize());
    out
}

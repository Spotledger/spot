//! Deterministic ids, hashes and the idempotency key: nothing here reads a clock or a random
//! source.

use super::*;

/// The deterministic idempotency key: DocType, event, the draft plus the endpoint's patch, and
/// the scripts that ran (name and source).
pub(super) fn idempotency_key(
    def: &DocTypeDef,
    request: &RunRequest<'_>,
    ran: &[String],
) -> String {
    let mut text = String::new();
    text.push_str(def.name.as_str());
    text.push('|');
    text.push_str(&request.event.key());
    text.push('|');
    text.push_str(&serde_json::to_string(request.server_patch).unwrap_or_default());
    text.push('|');
    text.push_str(
        &serde_json::to_string(&document_json_for_hash(request.document)).unwrap_or_default(),
    );
    text.push('|');
    for name in ran {
        text.push_str(name);
        text.push(',');
    }
    text.push('|');
    for script in request.scripts.scripts() {
        if ran.iter().any(|name| name == &script.name) {
            text.push_str(&script.name);
            text.push(':');
            text.push_str(&fnv1a(script.source.as_bytes()).to_string());
            text.push(';');
        }
    }
    format!("{:016x}", fnv1a(text.as_bytes()))
}

fn document_json_for_hash(doc: &Document) -> Json {
    let mut object = Map::new();
    object.insert("id".to_owned(), Json::String(doc.id().to_string()));
    for (name, value) in doc.fields() {
        object.insert(name.as_str().to_owned(), value_json(value));
    }
    for (table, rows) in doc.tables() {
        let encoded: Vec<Json> = rows
            .iter()
            .map(|row| {
                let mut entry = Map::new();
                entry.insert("id".to_owned(), Json::String(row.id.to_string()));
                for (name, value) in &row.fields {
                    entry.insert(name.as_str().to_owned(), value_json(value));
                }
                Json::Object(entry)
            })
            .collect();
        object.insert(table.as_str().to_owned(), Json::Array(encoded));
    }
    Json::Object(object)
}

/// A deterministic UUIDv7-shaped id from `seed` (FNV-1a over two lanes), as the form-event
/// adapter derives draft row ids. The version nibble is 7, which is what `RecordId` checks.
pub(super) fn stable_id(seed: &str) -> RecordId {
    let mut a: u64 = 0xcbf2_9ce4_8422_2325;
    let mut b: u64 = 0x8422_2325_cbf2_9ce4;
    for byte in seed.as_bytes() {
        a ^= u64::from(*byte);
        a = a.wrapping_mul(0x0000_0100_0000_01b3);
        b = b.rotate_left(7) ^ u64::from(*byte).wrapping_add(a);
    }
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.to_be_bytes());
    bytes[8..].copy_from_slice(&b.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    match RecordId::from_bytes(bytes) {
        Ok(id) => id,
        // Unreachable: the version nibble above is 7, which is all the constructor checks.
        Err(_) => RecordId::new_v7(),
    }
}

pub(super) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(super) fn seed_for(name: &str, event: &FormEvent, doc: &Document) -> u64 {
    let mut text = format!("{name}|{}|{}", event.key(), doc.id());
    if let Some(version) = doc.version() {
        text.push_str(&format!("|{}", version.0));
    }
    fnv1a(text.as_bytes())
}

pub(super) fn now_millis(now: OffsetDateTime) -> i64 {
    now.unix_timestamp()
        .saturating_mul(1_000)
        .saturating_add(i64::from(now.nanosecond() / 1_000_000))
}

/// Builds a document from a value map, for tests and the corpus: the same conversion the
/// lifecycle tier uses.
///
/// # Errors
///
/// `document.unknown_field` or `document.invalid_value` when the map does not fit `def`.
pub fn document_for(def: &DocTypeDef, values: &Json, id: RecordId) -> Result<Document, String> {
    let seed = id.to_string();
    let mut next = 0_u64;
    let patch = patch_from_json(def, values, || {
        let row = stable_id(&format!("{seed}|row|{next}"));
        next += 1;
        row
    })
    .map_err(|e| e.message.clone())?;
    let mut doc = Document::new(def.name.clone(), id);
    for (name, value) in patch.fields {
        doc.set(name, value);
    }
    for (table, rows) in patch.tables {
        let target = doc.rows_mut(table);
        for row in rows {
            target.push(ChildRow {
                id: row
                    .id
                    .unwrap_or_else(|| stable_id(&format!("{seed}|late|{next}"))),
                fields: row.fields,
            });
        }
    }
    Ok(doc)
}

/// A deterministic record id from a seed: the corpus and a host that needs a repeatable draft
/// id use it instead of a random one.
#[must_use]
pub fn id_for(seed: &str) -> RecordId {
    stable_id(seed)
}

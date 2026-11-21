//! Reading an offer from the wire: the `data` object of `GET /agent/v1/update`
//! (`AgentUpdateManifest` of contract `agent-channel`: `manifest`, `proof`,
//! `trust_anchor_id`) and, additively, a `revocations` object.
//!
//! `manifest` is either the `update.toml` text or a JSON object with the same fields; the
//! object is turned into exactly the TOML `spot-updater` parses, field by field, so unknown
//! fields, wrong types and floats are refused here and again by the updater's own parser.
//! `proof` is the text of `update.sig`. An absent or empty `proof` is an unsigned offer: it
//! parses (so the refusal is the updater's `Unsigned`, with its stable code) and is never
//! installed.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use spot_updater::{
    KeyId, MANIFEST_FORMAT, RevokedUpdateKey, SIGNATURE_BYTES, UpdateManifest, UpdateRevocation,
    UpdateRevocationBody, UpdateSignature,
};

use super::error::UpdateError;

/// Largest accepted `proof`, as the contract says.
pub const MAX_PROOF: usize = 1024;
/// Largest accepted manifest text or object, in serialised bytes.
pub const MAX_MANIFEST: usize = 256 * 1024;
/// Most revoked keys a list may carry.
pub const MAX_REVOKED: usize = 1024;

/// What the gateway (or a repository) offers.
#[derive(Clone, Debug)]
pub struct Offer {
    /// The offered manifest.
    pub manifest: UpdateManifest,
    /// The detached signature; `None` when the offer is unsigned.
    pub signature: Option<UpdateSignature>,
    /// A revocation list that came with the offer, if any.
    pub revocations: Option<UpdateRevocation>,
}

/// Parses the `data` object of an update answer.
///
/// # Errors
/// [`UpdateError::Wire`] naming the part that is malformed: `offer`, `manifest`, `proof` or
/// `revocations`.
pub fn parse_offer(data: &Value) -> Result<Offer, UpdateError> {
    let object = data.as_object().ok_or(UpdateError::Wire("offer"))?;
    let text = match object.get("manifest") {
        Some(Value::String(text)) if text.len() <= MAX_MANIFEST => text.clone(),
        Some(Value::Object(map)) => manifest_toml(map)?,
        _ => return Err(UpdateError::Wire("manifest")),
    };
    let manifest = UpdateManifest::from_toml(&text).map_err(|_| UpdateError::Wire("manifest"))?;
    let signature = match object.get("proof") {
        None | Some(Value::Null) => None,
        Some(Value::String(proof)) if proof.trim().is_empty() => None,
        Some(Value::String(proof)) if proof.len() <= MAX_PROOF => {
            Some(UpdateSignature::from_toml(proof).map_err(|_| UpdateError::Wire("proof"))?)
        }
        Some(_) => return Err(UpdateError::Wire("proof")),
    };
    let revocations = match object.get("revocations") {
        None | Some(Value::Null) => None,
        Some(value) => Some(revocation_from_json(value)?),
    };
    Ok(Offer {
        manifest,
        signature,
        revocations,
    })
}

const MANIFEST_FIELDS: &[&str] = &[
    "format",
    "product",
    "channel",
    "version",
    "min_from",
    "via",
    "released_at",
    "payload",
    "files",
    "migrations",
    "requires_os",
    "schema_epoch",
    "notes",
];

fn bad() -> UpdateError {
    UpdateError::Wire("manifest")
}

fn string_of<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a str, UpdateError> {
    map.get(key).and_then(Value::as_str).ok_or_else(bad)
}

fn int_of(map: &Map<String, Value>, key: &str) -> Result<i64, UpdateError> {
    map.get(key).and_then(Value::as_i64).ok_or_else(bad)
}

fn uint_of(map: &Map<String, Value>, key: &str) -> Result<u64, UpdateError> {
    map.get(key).and_then(Value::as_u64).ok_or_else(bad)
}

/// One TOML basic string, with escaping done by the `toml` crate.
fn quoted(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

fn only_keys(map: &Map<String, Value>, allowed: &[&str]) -> Result<(), UpdateError> {
    if map.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(bad())
    }
}

/// Builds the `update.toml` text of a manifest object.
fn manifest_toml(map: &Map<String, Value>) -> Result<String, UpdateError> {
    only_keys(map, MANIFEST_FIELDS)?;
    let mut out = String::new();
    let format = match map.get("format") {
        None => MANIFEST_FORMAT,
        Some(value) => value.as_str().ok_or_else(bad)?,
    };
    out.push_str(&format!("format = {}\n", quoted(format)));
    for key in ["product", "channel", "version", "min_from"] {
        out.push_str(&format!("{key} = {}\n", quoted(string_of(map, key)?)));
    }
    for key in ["requires_os", "notes"] {
        match map.get(key) {
            None | Some(Value::Null) => {}
            Some(value) => {
                out.push_str(&format!(
                    "{key} = {}\n",
                    quoted(value.as_str().ok_or_else(bad)?)
                ));
            }
        }
    }
    out.push_str(&format!("released_at = {}\n", int_of(map, "released_at")?));
    out.push_str(&format!(
        "schema_epoch = {}\n",
        uint_of(map, "schema_epoch")?
    ));
    if let Some(via) = map.get("via").filter(|v| !v.is_null()) {
        let items = via.as_array().ok_or_else(bad)?;
        let texts = items
            .iter()
            .map(|item| item.as_str().map(quoted).ok_or_else(bad))
            .collect::<Result<Vec<_>, _>>()?;
        out.push_str(&format!("via = [{}]\n", texts.join(", ")));
    }
    if let Some(migrations) = map.get("migrations").filter(|v| !v.is_null()) {
        let items = migrations.as_array().ok_or_else(bad)?;
        let numbers = items
            .iter()
            .map(|item| item.as_u64().map(|n| n.to_string()).ok_or_else(bad))
            .collect::<Result<Vec<_>, _>>()?;
        out.push_str(&format!("migrations = [{}]\n", numbers.join(", ")));
    }
    let payload = map
        .get("payload")
        .and_then(Value::as_object)
        .ok_or_else(bad)?;
    only_keys(payload, &["file", "size", "sha256"])?;
    out.push_str("\n[payload]\n");
    out.push_str(&format!("file = {}\n", quoted(string_of(payload, "file")?)));
    out.push_str(&format!("size = {}\n", uint_of(payload, "size")?));
    out.push_str(&format!(
        "sha256 = {}\n",
        quoted(string_of(payload, "sha256")?)
    ));
    if let Some(files) = map.get("files").filter(|v| !v.is_null()) {
        for file in files.as_array().ok_or_else(bad)? {
            let file = file.as_object().ok_or_else(bad)?;
            only_keys(file, &["path", "size", "sha256"])?;
            out.push_str("\n[[files]]\n");
            out.push_str(&format!("path = {}\n", quoted(string_of(file, "path")?)));
            out.push_str(&format!("size = {}\n", uint_of(file, "size")?));
            out.push_str(&format!(
                "sha256 = {}\n",
                quoted(string_of(file, "sha256")?)
            ));
        }
    }
    Ok(out)
}

fn rev_bad() -> UpdateError {
    UpdateError::Wire("revocations")
}

/// Reads a revocation list: `generation`, `issued_at`, `issued_by`, `keys` (`key_id`,
/// `reason`) and the base64 `signature`.
///
/// # Errors
/// [`UpdateError::Wire`] (`revocations`) for any malformed part. The signature is checked
/// later, against the trust anchor.
pub fn revocation_from_json(value: &Value) -> Result<UpdateRevocation, UpdateError> {
    let object = value.as_object().ok_or_else(rev_bad)?;
    only_keys(
        object,
        &["generation", "issued_at", "issued_by", "keys", "signature"],
    )
    .map_err(|_| rev_bad())?;
    let generation = object
        .get("generation")
        .and_then(Value::as_u64)
        .ok_or_else(rev_bad)?;
    let issued_at = object
        .get("issued_at")
        .and_then(Value::as_i64)
        .ok_or_else(rev_bad)?;
    let issued_by = KeyId::parse(
        object
            .get("issued_by")
            .and_then(Value::as_str)
            .ok_or_else(rev_bad)?,
    )
    .map_err(|_| rev_bad())?;
    let listed = object
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(rev_bad)?;
    if listed.len() > MAX_REVOKED {
        return Err(rev_bad());
    }
    let mut keys = Vec::with_capacity(listed.len());
    for entry in listed {
        let entry = entry.as_object().ok_or_else(rev_bad)?;
        only_keys(entry, &["key_id", "reason"]).map_err(|_| rev_bad())?;
        let key_id = KeyId::parse(
            entry
                .get("key_id")
                .and_then(Value::as_str)
                .ok_or_else(rev_bad)?,
        )
        .map_err(|_| rev_bad())?;
        let reason = entry
            .get("reason")
            .and_then(Value::as_str)
            .ok_or_else(rev_bad)?
            .to_owned();
        keys.push(RevokedUpdateKey { key_id, reason });
    }
    // The signed bytes list the keys sorted by id; a list sent in another order is the same
    // list, so sort before it is signed over.
    keys.sort_by(|a, b| a.key_id.cmp(&b.key_id));
    let bytes = STANDARD
        .decode(
            object
                .get("signature")
                .and_then(Value::as_str)
                .ok_or_else(rev_bad)?,
        )
        .map_err(|_| rev_bad())?;
    let signature = <[u8; SIGNATURE_BYTES]>::try_from(bytes.as_slice()).map_err(|_| rev_bad())?;
    Ok(UpdateRevocation::from_parts(
        UpdateRevocationBody {
            generation,
            issued_at,
            issued_by,
            keys,
        },
        signature,
    ))
}

/// The JSON form of a revocation list, as [`revocation_from_json`] reads it.
#[must_use]
pub fn revocation_to_json(list: &UpdateRevocation) -> Value {
    let keys: Vec<Value> = list
        .body
        .keys
        .iter()
        .map(|key| json!({"key_id": key.key_id.as_str(), "reason": key.reason}))
        .collect();
    json!({
        "generation": list.body.generation,
        "issued_at": list.body.issued_at,
        "issued_by": list.body.issued_by.as_str(),
        "keys": keys,
        "signature": STANDARD.encode(list.signature()),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_updater::SigningKey;

    use super::*;

    const DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn manifest_object() -> Value {
        json!({
            "product": "spot-agent",
            "channel": "stable",
            "version": "2.0.0",
            "min_from": "1.0.0",
            "via": [],
            "released_at": 1_700_000_000,
            "payload": {"file": "spot-agent-2.0.0.zip", "size": 10, "sha256": DIGEST},
            "files": [{"path": "spot-agent", "size": 4, "sha256": DIGEST}],
            "migrations": [],
            "requires_os": null,
            "schema_epoch": 0,
        })
    }

    fn list(generation: u64, seeds: &[u8]) -> UpdateRevocation {
        let signing = SigningKey::from_seed(&[9; 32]);
        UpdateRevocation::issue(
            UpdateRevocationBody {
                generation,
                issued_at: 0,
                issued_by: signing.public().key_id(),
                keys: seeds
                    .iter()
                    .map(|seed| RevokedUpdateKey {
                        key_id: SigningKey::from_seed(&[*seed; 32]).public().key_id(),
                        reason: "compromised".to_owned(),
                    })
                    .collect(),
            },
            &signing,
        )
    }

    #[test]
    fn an_object_manifest_parses_with_every_field() {
        let offer = parse_offer(&json!({"manifest": manifest_object()})).unwrap();
        assert_eq!(offer.manifest.product, "spot-agent");
        assert_eq!(offer.manifest.version.to_string(), "2.0.0");
        assert_eq!(offer.manifest.payload.size, 10);
        assert_eq!(offer.manifest.files.len(), 1);
        assert_eq!(offer.manifest.schema_epoch, 0);
        assert!(offer.signature.is_none(), "no proof is an unsigned offer");
        assert!(offer.revocations.is_none());
    }

    #[test]
    fn a_manifest_may_also_arrive_as_update_toml_text() {
        let text = format!(
            "format = \"spot-update-manifest\"\nproduct = \"spot-agent\"\nchannel = \"stable\"\n\
             version = \"2.0.0\"\nmin_from = \"1.0.0\"\nreleased_at = 1700000000\n\
             schema_epoch = 0\n[payload]\nfile = \"p.zip\"\nsize = 1\nsha256 = \"{DIGEST}\"\n"
        );
        let offer = parse_offer(&json!({"manifest": text, "proof": ""})).unwrap();
        assert_eq!(offer.manifest.version.to_string(), "2.0.0");
        assert!(offer.signature.is_none());
    }

    #[test]
    fn unknown_fields_floats_and_wrong_types_are_refused() {
        let mut extra = manifest_object();
        extra["surprise"] = json!(1);
        let mut float = manifest_object();
        float["released_at"] = json!(1.5);
        let mut wrong = manifest_object();
        wrong["version"] = json!(2);
        let mut nested = manifest_object();
        nested["payload"]["extra"] = json!(true);
        for (value, part) in [
            (json!({"manifest": extra}), "manifest"),
            (json!({"manifest": float}), "manifest"),
            (json!({"manifest": wrong}), "manifest"),
            (json!({"manifest": nested}), "manifest"),
            (
                json!({"manifest": manifest_object(), "proof": "not toml ["}),
                "proof",
            ),
            (
                json!({"manifest": manifest_object(), "proof": "x".repeat(2000)}),
                "proof",
            ),
            (json!({"manifest": manifest_object(), "proof": 5}), "proof"),
            (json!("string"), "offer"),
            (json!({}), "manifest"),
        ] {
            assert_eq!(
                parse_offer(&value).unwrap_err(),
                UpdateError::Wire(part),
                "{value}"
            );
        }
    }

    #[test]
    fn a_string_with_quotes_and_newlines_cannot_inject_toml() {
        let mut crafted = manifest_object();
        crafted["notes"] = json!("x\"\nschema_epoch = 9\n[payload]\nsize = 1");
        let offer = parse_offer(&json!({"manifest": crafted})).unwrap();
        assert_eq!(offer.manifest.schema_epoch, 0);
        assert_eq!(offer.manifest.payload.size, 10);
    }

    #[test]
    fn a_revocation_list_round_trips_through_json_and_rejects_garbage() {
        let original = list(3, &[1, 5]);
        assert_eq!(
            revocation_from_json(&revocation_to_json(&original)).unwrap(),
            original
        );
        let mut short = revocation_to_json(&original);
        short["signature"] = json!("AAAA");
        assert!(revocation_from_json(&short).is_err());
        let mut extra = revocation_to_json(&original);
        extra["x"] = json!(1);
        assert!(revocation_from_json(&extra).is_err());
        let mut badkey = revocation_to_json(&original);
        badkey["issued_by"] = json!("not-a-key-id");
        assert!(revocation_from_json(&badkey).is_err());
    }

    #[test]
    fn a_list_sent_in_another_order_is_the_same_list() {
        let original = list(1, &[1, 5, 7]);
        let mut value = revocation_to_json(&original);
        if let Some(keys) = value["keys"].as_array_mut() {
            keys.reverse();
        }
        assert_eq!(revocation_from_json(&value).unwrap(), original);
    }
}

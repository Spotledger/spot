//! The EDI round trip on a running server (WP-INT-06.T2). The shared fixture host mounts no
//! `edi` connector (the connector service registers the kind in a host task and the ingress is a
//! public route a host mounts), so the case is named and **blocked** with the observation that
//! proves the surface is missing. The behaviour itself is covered by `spot-cli`'s
//! `tests/edi_connector.rs`.

use super::CaseDef;

/// After the other groups.
pub const ORDER: u32 = 41;

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::blocked(
        "edi: an_inbound_x12_850_creates_the_documents_as_the_service_principal_and_a_997_is_queued",
        "EDI",
        "an interchange posted to `POST /api/v1/connectors/{name}/inbound` creates mapped documents as the connector's principal, a repeat control number is acknowledged as rejected, and the acknowledgement is queued",
        "WP-INT-06.T2 (host registration of the `edi` kind and the ingress mount)",
        "`POST /api/v1/connectors/{name}/inbound` is not mounted by the host and the connector service does not register `edi` yet",
    )]
}

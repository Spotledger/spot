//! Two sites on one host, for the cross-tenant cases of WP-BPM-02.T8.

use spot_auth::Password;
use spot_cli::SiteKind;

use crate::client::{Client, Reply};
use crate::failure;
use crate::fixture::{ADMIN_PASSWORD, Fixture};
use crate::report::Failure;

use super::super::process::support::Host;

/// Creates a second site on the fixture's root (administrator password chosen) and starts one host
/// serving both. Returns the host and the second site's name.
pub async fn two_sites(fixture: &Fixture) -> Result<(Host, String), Failure> {
    let other = format!("{}-b", fixture.site());
    spot_cli::new_site_of(fixture.root(), &other, "en", SiteKind::HostWide)
        .await
        .map_err(|e| failure!("new-site `{other}`: {e}"))?;
    // The site's own key-encryption file, as the fixture makes the first site's.
    spot_cli::services::secrets::init_key_file(
        &fixture.root().join(format!("keys/{other}.json")),
        &other,
    )
    .await
    .map_err(|e| failure!("key file of `{other}`: {e}"))?;
    spot_cli::users::set_password(
        fixture.root(),
        &other,
        "administrator",
        &Password::new(ADMIN_PASSWORD),
    )
    .await
    .map_err(|e| failure!("set-password on `{other}`: {e}"))?;
    let host = Host::restart(fixture).await?;
    Ok((host, other))
}

/// What a reply is, for "byte for byte" comparisons: the status, the body without its request id,
/// and the headers that describe caching and content.
#[derive(Debug, PartialEq, Eq)]
pub struct Shape {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

/// The comparable shape of `reply`.
#[must_use]
pub fn shape(reply: &Reply) -> Shape {
    let mut body = String::from_utf8_lossy(&reply.body).into_owned();
    if let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(&reply.body) {
        if let Some(object) = json.as_object_mut() {
            object.remove("request_id");
            if let Some(error) = object.get_mut("error").and_then(|e| e.as_object_mut()) {
                error.remove("request_id");
            }
        }
        body = json.to_string();
    }
    let mut headers: Vec<(String, String)> = reply
        .headers
        .iter()
        .filter(|(name, _)| {
            matches!(
                name.as_str(),
                "cache-control" | "content-type" | "www-authenticate"
            )
        })
        .cloned()
        .collect();
    headers.sort();
    Shape {
        status: reply.status,
        body,
        headers,
    }
}

/// A client on `site` of `host`, signed in as the Administrator.
pub async fn admin_of(host: &Host, site: &str) -> Result<Client, Failure> {
    host.client(site)
        .sign_in("administrator", ADMIN_PASSWORD)
        .await
}

//! What the service and the `spot egress` command share: where the files are, the one-shot
//! `check`, and the reload marker.
//!
//! `check` never opens a connection: it resolves the host, classifies every answer and asks the
//! same guard code the server uses, with a private in-memory sink so that an operator's probe is
//! not recorded as a tenant's denial.

use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use spot_core::Clock;
use spot_egress::{DenialSink, EgressPolicy, Guard, InMemorySink, Purpose, Request, Resolver};

use super::denials::class_name;
use super::{Settings, codes, refuse};
use crate::Result;

/// The default denial log, in the site's folder.
const DEFAULT_LOG: &str = "egress-denials.log";

/// The files of one site's egress service.
#[derive(Clone, Debug)]
pub struct Paths {
    /// The policy file, when one is configured.
    pub policy_file: Option<PathBuf>,
    /// The denial log.
    pub denial_log: PathBuf,
    /// The marker `spot egress reload` writes and the running service consumes.
    pub reload_marker: PathBuf,
}

fn resolve_one(key: &str, template: &Path, root: Option<&Path>, site: &str) -> Result<PathBuf> {
    let path = PathBuf::from(template.to_string_lossy().replace("{site}", site));
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(refuse(
            codes::PATH_REFUSED,
            format!("services.egress.{key} must not contain `..`"),
        ));
    }
    if path.is_absolute() {
        return Ok(path);
    }
    match root {
        Some(root) => Ok(root.join(path)),
        None => Err(refuse(
            codes::PATH_REFUSED,
            format!("services.egress.{key} must be an absolute path here"),
        )),
    }
}

impl Paths {
    /// The paths for `site`. A relative path is relative to the host root; `{site}` is the site's
    /// name. The default denial log is `<root>/sites/<site>/egress-denials.log`. With several
    /// sites an explicit `denial_log` must contain `{site}`: two sites must not share one file.
    ///
    /// # Errors
    ///
    /// `egress_host.path_refused`.
    pub fn resolve(
        settings: &Settings,
        root: Option<&Path>,
        site: &str,
        sites: usize,
    ) -> Result<Self> {
        let policy_file = match &settings.policy_file {
            Some(template) => Some(resolve_one("policy_file", template, root, site)?),
            None => None,
        };
        let denial_log = match &settings.denial_log {
            Some(template) => {
                if sites > 1 && !template.to_string_lossy().contains("{site}") {
                    return Err(refuse(
                        codes::PATH_REFUSED,
                        "services.egress.denial_log is shared by several sites; put `{site}` in the path",
                    ));
                }
                resolve_one("denial_log", template, root, site)?
            }
            None => match root {
                Some(root) => root.join("sites").join(site).join(DEFAULT_LOG),
                None => {
                    return Err(refuse(
                        codes::PATH_REFUSED,
                        "services.egress.denial_log is needed when the site has no folder",
                    ));
                }
            },
        };
        let mut marker = denial_log.clone().into_os_string();
        marker.push(".reload");
        Ok(Self {
            policy_file,
            denial_log,
            reload_marker: PathBuf::from(marker),
        })
    }
}

// ---- the reload marker -----------------------------------------------------------------------

/// Asks the running service to re-read its policy file.
///
/// # Errors
///
/// The marker cannot be created.
pub fn request_reload(marker: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(marker).map(|_| ())
}

/// Consumes the marker; `true` when one was there.
pub fn take_reload(marker: &Path) -> bool {
    std::fs::remove_file(marker).is_ok()
}

// ---- the one-shot check ----------------------------------------------------------------------

/// What `spot egress check` asks.
#[derive(Clone, Debug)]
pub struct CheckRequest {
    /// The destination. Only its scheme, host and port are used.
    pub url: url::Url,
    /// Why the caller would connect.
    pub purpose: Purpose,
    /// The tenant (the site's name).
    pub tenant: String,
}

/// The answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The guard would let the caller connect, to exactly these addresses.
    Allowed {
        /// `http` or `https`.
        scheme: String,
        /// The host as given.
        host: String,
        /// The port.
        port: u16,
        /// The addresses a caller would be pinned to.
        addrs: Vec<IpAddr>,
    },
    /// The guard refuses.
    Denied {
        /// The catalogue code (`egress.denied_by_policy` and the like).
        code: &'static str,
        /// The rule: `policy`, `address`, `port`, `resolution` or `url`.
        rule: String,
        /// The address class, for an `address` refusal.
        class: Option<String>,
    },
}

/// Decides one destination under `policy`, resolving with `resolver`. Opens no connection.
pub async fn check(
    policy: EgressPolicy,
    resolver: Arc<dyn Resolver>,
    clock: Arc<dyn Clock>,
    request: &CheckRequest,
) -> CheckOutcome {
    let sink = Arc::new(InMemorySink::new());
    let guard = Guard::new(policy, resolver, sink.clone() as Arc<dyn DenialSink>, clock);
    let asked = Request {
        url: request.url.clone(),
        purpose: request.purpose,
        tenant: request.tenant.clone(),
        actor: Some("operator check".to_owned()),
    };
    match guard.authorize(&asked).await {
        Ok(authorized) => CheckOutcome::Allowed {
            scheme: request.url.scheme().to_owned(),
            host: authorized.host().to_owned(),
            port: request.url.port_or_known_default().unwrap_or(0),
            addrs: authorized.socket_addrs().iter().map(|a| a.ip()).collect(),
        },
        Err(error) => {
            let record = sink.records().pop();
            let rule = record
                .as_ref()
                .map_or_else(|| "url".to_owned(), |r| r.rule.clone());
            let class = record
                .filter(|r| r.rule == "address")
                .map(|r| class_name(r.class));
            CheckOutcome::Denied {
                code: error.code(),
                rule,
                class,
            }
        }
    }
}

/// The purpose a name stands for (`webhook`, `mail`, ...; `app` is app 0 here).
pub fn parse_purpose(name: &str) -> Option<Purpose> {
    Some(match name {
        "webhook" => Purpose::Webhook,
        "script" => Purpose::Script,
        "connector" => Purpose::Connector,
        "mail" => Purpose::Mail,
        "llm" => Purpose::Llm,
        "template" => Purpose::Template,
        "pdf" => Purpose::Pdf,
        "app" => Purpose::App(0),
        _ => return None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_default_log_is_in_the_site_folder_and_the_marker_sits_beside_it() {
        let paths = Paths::resolve(
            &Settings::default(),
            Some(Path::new("/srv/spot")),
            "acme",
            1,
        )
        .unwrap();
        assert_eq!(
            paths.denial_log,
            Path::new("/srv/spot/sites/acme/egress-denials.log")
        );
        assert!(
            paths
                .reload_marker
                .to_string_lossy()
                .ends_with("egress-denials.log.reload")
        );
    }

    #[test]
    fn a_shared_log_or_a_parent_component_is_refused() {
        let shared = Settings {
            denial_log: Some(PathBuf::from("/var/log/egress.log")),
            ..Settings::default()
        };
        assert!(Paths::resolve(&shared, None, "a", 2).is_err());
        assert!(Paths::resolve(&shared, None, "a", 1).is_ok());
        let up = Settings {
            policy_file: Some(PathBuf::from("/etc/../shadow")),
            denial_log: Some(PathBuf::from("/var/log/egress.log")),
            ..Settings::default()
        };
        assert!(Paths::resolve(&up, None, "a", 1).is_err());
    }

    #[test]
    fn purposes_round_trip() {
        for name in super::super::policy::PURPOSES {
            assert_eq!(parse_purpose(name).unwrap().as_str(), name);
        }
        assert!(parse_purpose("telnet").is_none());
    }
}

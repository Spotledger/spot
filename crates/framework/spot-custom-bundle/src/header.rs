//! `bundle.toml`: the bundle's own metadata and the index of its sections.

use serde::{Deserialize, Serialize};

/// The only format version this crate writes and the newest it reads.
pub const FORMAT: u32 = 1;

/// The required metadata entry.
pub const BUNDLE_TOML: &str = "bundle.toml";

/// The required content-address entry.
pub const DIGEST_ENTRY: &str = "digest";

/// The directory every section lives under.
pub const SECTIONS_DIR: &str = "sections";

/// One app and the version the bundle was captured against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppVersion {
    /// The app's name.
    pub app: String,
    /// The app's version at capture time.
    pub version: String,
}

/// One section's index entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionRef {
    /// The section kind (its directory name).
    pub kind: String,
    /// The section's directory, `sections/<kind>`.
    pub path: String,
    /// The section's schema version.
    pub schema: u32,
    /// SHA-256 of the section's canonical file listing.
    pub sha256: String,
}

/// The parsed `bundle.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleHeader {
    /// The format version.
    pub format: u32,
    /// The bundle's name.
    pub name: String,
    /// The bundle's own version (semver, as text).
    pub bundle_version: String,
    /// Who created it.
    pub created_by: String,
    /// When it was created. An argument, never read from the clock.
    pub created_at: String,
    /// The host/platform version it was captured on.
    pub host_version: String,
    /// The installed apps and versions at capture time, sorted by app name.
    #[serde(default)]
    pub captured_against: Vec<AppVersion>,
    /// The sections the bundle carries, sorted by kind.
    #[serde(default)]
    pub sections: Vec<SectionRef>,
}

impl BundleHeader {
    /// A header with the fixed format and every list sorted, so the TOML is canonical.
    pub fn new(
        name: impl Into<String>,
        bundle_version: impl Into<String>,
        created_by: impl Into<String>,
        created_at: impl Into<String>,
        host_version: impl Into<String>,
        captured_against: Vec<AppVersion>,
    ) -> Self {
        let mut captured_against = captured_against;
        captured_against.sort_by(|a, b| a.app.cmp(&b.app));
        Self {
            format: FORMAT,
            name: name.into(),
            bundle_version: bundle_version.into(),
            created_by: created_by.into(),
            created_at: created_at.into(),
            host_version: host_version.into(),
            captured_against,
            sections: Vec::new(),
        }
    }

    /// Sets the sections, sorted by kind, and returns `self`.
    pub fn with_sections(mut self, mut sections: Vec<SectionRef>) -> Self {
        sections.sort_by(|a, b| a.kind.cmp(&b.kind));
        self.sections = sections;
        self
    }

    /// The serialized TOML, canonical because every list is sorted.
    pub fn to_toml(&self) -> Result<String, crate::error::CustomBundleError> {
        toml::to_string_pretty(self).map_err(|err| {
            crate::error::CustomBundleError::new(
                crate::error::codes::MISSING_ENTRY,
                format!("the bundle header could not be written: {err}"),
            )
        })
    }

    /// Parses the header, refusing a format newer than [`FORMAT`].
    pub fn parse(text: &str) -> Result<Self, crate::error::CustomBundleError> {
        let header: Self = toml::from_str(text).map_err(|err| {
            crate::error::CustomBundleError::about(
                crate::error::codes::MISSING_ENTRY,
                BUNDLE_TOML,
                format!("the header is not valid TOML: {}", err.message().trim_end()),
            )
        })?;
        if header.format > FORMAT {
            return Err(crate::error::CustomBundleError::about(
                crate::error::codes::FORMAT_TOO_NEW,
                BUNDLE_TOML,
                format!(
                    "the bundle's format is {}; this reader supports {}",
                    header.format, FORMAT
                ),
            ));
        }
        Ok(header)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips_and_sorts() {
        let header = BundleHeader::new(
            "site-a",
            "1.0.0",
            "glm",
            "2026-01-01T00:00:00Z",
            "0.1.0",
            vec![
                AppVersion {
                    app: "crm".to_owned(),
                    version: "1.0.0".to_owned(),
                },
                AppVersion {
                    app: "accounting".to_owned(),
                    version: "2.3.4".to_owned(),
                },
            ],
        )
        .with_sections(vec![
            SectionRef {
                kind: "views".to_owned(),
                path: "sections/views".to_owned(),
                schema: 1,
                sha256: "00".to_owned(),
            },
            SectionRef {
                kind: "meta".to_owned(),
                path: "sections/meta".to_owned(),
                schema: 1,
                sha256: "11".to_owned(),
            },
        ]);
        let text = header.to_toml().expect("header writes");
        let parsed = BundleHeader::parse(&text).expect("header parses");
        assert_eq!(parsed, header);
        assert_eq!(parsed.captured_against[0].app, "accounting");
        assert_eq!(parsed.sections[0].kind, "meta");
    }

    #[test]
    fn newer_format_is_refused() {
        let text = "format = 99\nname = \"x\"\nbundle_version = \"1.0.0\"\ncreated_by = \"g\"\n\
             created_at = \"t\"\nhost_version = \"0.1.0\"\n";
        let err = BundleHeader::parse(text).expect_err("too new");
        assert_eq!(err.code(), crate::error::codes::FORMAT_TOO_NEW);
    }
}

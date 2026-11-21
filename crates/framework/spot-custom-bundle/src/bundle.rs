//! The bundle itself: assembling, reading and re-packing the deterministic ZIP.

use std::collections::{BTreeMap, BTreeSet};

use crate::digest::{Digest, digest_of};
use crate::error::{CustomBundleError, codes};
use crate::header::{BUNDLE_TOML, BundleHeader, DIGEST_ENTRY, SectionRef};
use crate::limits::Limits;
use crate::paths::normalise;
use crate::registry::{Files, Problem, SectionRegistry};

/// One section's files and schema version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionData {
    /// The section's schema version.
    pub schema: u32,
    /// The section's files, keyed relative to its directory.
    pub files: Files,
}

/// A customization bundle, held in memory.
///
/// Built by [`Bundle::pack`] or [`Bundle::open`]; the digest and the section index are
/// derived, never edited from outside.
#[derive(Clone, Debug)]
pub struct Bundle {
    /// The parsed header.
    header: BundleHeader,
    /// The canonical digest.
    digest: Digest,
    /// The sections by kind, in pack order.
    sections: BTreeMap<String, SectionData>,
    /// The section kinds this reader does not know (carried opaquely).
    unknown: BTreeSet<String>,
}

/// The SHA-256 of one section's canonical file listing, as hex.
fn section_sha(section: &SectionData) -> String {
    let parts: Vec<(String, Vec<u8>)> = section
        .files
        .iter()
        .map(|(name, bytes)| (name.clone(), bytes.clone()))
        .collect();
    digest_of(&parts).to_hex()
}

/// Rebuilds the full entry map (everything in the digest input) from a header and sections.
fn assemble_entries(
    header: &BundleHeader,
    sections: &BTreeMap<String, SectionData>,
) -> Result<BTreeMap<String, Vec<u8>>, CustomBundleError> {
    let mut entries = BTreeMap::new();
    entries.insert(BUNDLE_TOML.to_owned(), header.to_toml()?.into_bytes());
    for (kind, section) in sections {
        for (name, bytes) in &section.files {
            entries.insert(format!("sections/{kind}/{name}"), bytes.clone());
        }
    }
    Ok(entries)
}

/// The digest over an entry map: everything except the `digest` entry itself.
fn digest_of_entries(entries: &BTreeMap<String, Vec<u8>>) -> Digest {
    let parts: Vec<(String, Vec<u8>)> = entries
        .iter()
        .filter(|(name, _)| name.as_str() != DIGEST_ENTRY)
        .map(|(name, bytes)| (name.clone(), bytes.clone()))
        .collect();
    digest_of(&parts)
}

impl Bundle {
    /// Packs a bundle from a header and its sections, deterministically.
    ///
    /// The header's `sections` list is recomputed from `sections`, so the section index and
    /// the files can never disagree. A section kind with no files is still indexed (an empty
    /// section is meaningful: applying it removes everything).
    pub fn pack(
        header: BundleHeader,
        sections: BTreeMap<String, SectionData>,
        limits: &Limits,
    ) -> Result<Bundle, CustomBundleError> {
        let refs: Vec<SectionRef> = sections
            .iter()
            .map(|(kind, section)| SectionRef {
                kind: kind.clone(),
                path: format!("sections/{kind}"),
                schema: section.schema,
                sha256: section_sha(section),
            })
            .collect();
        let header = header.with_sections(refs);
        let mut entries = assemble_entries(&header, &sections)?;
        let digest = digest_of_entries(&entries);
        entries.insert(DIGEST_ENTRY.to_owned(), digest.to_hex().into_bytes());
        let bytes = crate::zip::write_zip(&entries)?;
        if bytes.len() as u64 > limits.max_compressed_bytes {
            return Err(CustomBundleError::new(
                codes::TOO_LARGE,
                format!(
                    "the packed bundle is {} bytes; the limit is {}",
                    bytes.len(),
                    limits.max_compressed_bytes
                ),
            ));
        }
        Ok(Bundle {
            header,
            digest,
            sections,
            unknown: BTreeSet::new(),
        })
    }

    /// Opens and validates a bundle from bytes.
    ///
    /// Checks the ZIP limits and path safety, then: `bundle.toml` and `digest` are present and
    /// parse; every other entry is under `sections/`; the stored digest matches the content;
    /// every known section's `schema` is one this reader supports. A section kind not in
    /// `registry` is refused with `custom_bundle.unknown_section` unless `allow_unknown`, in
    /// which case its files are carried byte-for-byte so a later re-pack is unchanged.
    pub fn open(
        bytes: &[u8],
        limits: &Limits,
        registry: &SectionRegistry,
        allow_unknown: bool,
    ) -> Result<Bundle, Vec<CustomBundleError>> {
        let entries = crate::zip::read_zip(bytes, limits)?;
        let mut errors = Vec::new();

        let Some(header_bytes) = entries.get(BUNDLE_TOML) else {
            errors.push(CustomBundleError::new(
                codes::MISSING_ENTRY,
                "the bundle has no bundle.toml",
            ));
            return Err(errors);
        };
        let header = match std::str::from_utf8(header_bytes) {
            Ok(text) => match BundleHeader::parse(text) {
                Ok(header) => header,
                Err(err) => return Err(vec![err]),
            },
            Err(_) => {
                errors.push(CustomBundleError::about(
                    codes::MISSING_ENTRY,
                    BUNDLE_TOML,
                    "the header is not valid UTF-8",
                ));
                return Err(errors);
            }
        };

        let Some(digest_bytes) = entries.get(DIGEST_ENTRY) else {
            errors.push(CustomBundleError::new(
                codes::MISSING_ENTRY,
                "the bundle has no digest entry",
            ));
            return Err(errors);
        };
        let actual = digest_of_entries(&entries);
        match std::str::from_utf8(digest_bytes)
            .ok()
            .and_then(|text| text.trim().parse::<Digest>().ok())
        {
            Some(stored) if stored == actual => {}
            _ => {
                errors.push(CustomBundleError::about(
                    codes::DIGEST_MISMATCH,
                    DIGEST_ENTRY,
                    format!(
                        "the stored digest does not match the content's {}",
                        actual.to_hex()
                    ),
                ));
            }
        }

        // Every entry is bundle.toml, digest, or a file under sections/<kind>/.
        let mut sections: BTreeMap<String, SectionData> = BTreeMap::new();
        for name in entries.keys() {
            if name == BUNDLE_TOML || name == DIGEST_ENTRY {
                continue;
            }
            let Ok(safe) = normalise(name.as_bytes()) else {
                continue;
            };
            match crate::zip::split_section(&safe) {
                Some((kind, file)) => {
                    let section = sections.entry(kind.clone()).or_insert_with(|| SectionData {
                        schema: registry
                            .get(&kind)
                            .map_or(1, |section| section.schema_version()),
                        files: Files::new(),
                    });
                    section.files.insert(file, entries[name].clone());
                }
                None => errors.push(CustomBundleError::about(
                    codes::UNEXPECTED_ENTRY,
                    name.clone(),
                    "a bundle carries bundle.toml, digest and files under sections/<kind>/ only",
                )),
            }
        }

        // Every header section has files and a matching SHA; every section is known or allowed.
        let by_kind: BTreeMap<&str, &SectionRef> = header
            .sections
            .iter()
            .map(|reference| (reference.kind.as_str(), reference))
            .collect();
        let mut unknown = BTreeSet::new();
        for (kind, section) in &sections {
            match by_kind.get(kind.as_str()) {
                None => errors.push(CustomBundleError::about(
                    codes::UNEXPECTED_ENTRY,
                    format!("sections/{kind}"),
                    "the section is not listed in bundle.toml",
                )),
                Some(reference) => match registry.get(kind) {
                    Some(known) => {
                        if reference.schema > known.schema_version() {
                            errors.push(CustomBundleError::about(
                                codes::UNKNOWN_SECTION,
                                kind.clone(),
                                format!(
                                    "the section's schema is {}; this reader supports {}",
                                    reference.schema,
                                    known.schema_version()
                                ),
                            ));
                        }
                    }
                    None => {
                        if allow_unknown {
                            unknown.insert(kind.clone());
                        } else {
                            errors.push(CustomBundleError::about(
                                codes::UNKNOWN_SECTION,
                                kind.clone(),
                                "this reader does not know the section; set allow_unknown to \
                                 carry it through unchanged",
                            ));
                        }
                    }
                },
            }
            let expected = section_sha(section);
            if let Some(reference) = by_kind.get(kind.as_str())
                && reference.sha256 != expected
            {
                errors.push(CustomBundleError::about(
                    codes::DIGEST_MISMATCH,
                    format!("sections/{kind}"),
                    "the section's content does not match its recorded sha256",
                ));
            }
        }
        for reference in &header.sections {
            if !sections.contains_key(&reference.kind) {
                errors.push(CustomBundleError::about(
                    codes::MISSING_ENTRY,
                    format!("sections/{}", reference.kind),
                    "bundle.toml lists the section but the archive does not carry it",
                ));
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Bundle {
            header,
            digest: actual,
            sections,
            unknown,
        })
    }

    /// The parsed header.
    pub fn header(&self) -> &BundleHeader {
        &self.header
    }

    /// The canonical digest.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// One section's files, if the bundle carries the kind.
    pub fn section(&self, kind: &str) -> Option<&SectionData> {
        self.sections.get(kind)
    }

    /// The section kinds the bundle carries, in pack order.
    pub fn kinds(&self) -> Vec<&str> {
        self.sections.keys().map(String::as_str).collect()
    }

    /// The kinds this reader does not know (carried opaquely).
    pub fn unknown_kinds(&self) -> impl Iterator<Item = &str> {
        self.unknown.iter().map(String::as_str)
    }

    /// The section map.
    pub fn sections(&self) -> &BTreeMap<String, SectionData> {
        &self.sections
    }

    /// The archive bytes this bundle was opened from or packed to.
    pub fn to_bytes(&self) -> Vec<u8> {
        // Re-assembling is what makes re-pack byte-faithful; it is also cheap to verify, so
        // the OK path returns a freshly packed archive.
        self.repack().unwrap_or_default()
    }

    /// Re-packs the bundle from its header and sections.
    ///
    /// An unknown section is carried byte-for-byte: its header reference (schema included) is
    /// kept and its files are written back at the same names.
    pub fn repack(&self) -> Result<Vec<u8>, CustomBundleError> {
        let mut header = self.header.clone();
        let mut sections = self.sections.clone();
        // Preserve the original schema for a kind with no live section (an unknown kind).
        for reference in &self.header.sections {
            if let Some(section) = sections.get_mut(&reference.kind)
                && self.unknown.contains(&reference.kind)
            {
                section.schema = reference.schema;
            }
        }
        let refs: Vec<SectionRef> = sections
            .iter()
            .map(|(kind, section)| SectionRef {
                kind: kind.clone(),
                path: format!("sections/{kind}"),
                schema: section.schema,
                sha256: section_sha(section),
            })
            .collect();
        header = header.with_sections(refs);
        let mut entries = assemble_entries(&header, &sections)?;
        let digest = digest_of_entries(&entries);
        entries.insert(DIGEST_ENTRY.to_owned(), digest.to_hex().into_bytes());
        crate::zip::write_zip(&entries)
    }

    /// Runs every known section's `check` over the bundle's files.
    pub fn check(&self, registry: &SectionRegistry) -> Vec<Problem> {
        let mut problems = Vec::new();
        for section in registry.iter() {
            if let Some(data) = self.sections.get(section.kind()) {
                problems.extend(section.check(&data.files));
            }
        }
        problems
    }
}

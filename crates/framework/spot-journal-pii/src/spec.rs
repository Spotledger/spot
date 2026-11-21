//! What a personal field is: its path in the image, where the data-subject id sits, why the
//! value is held, which retention class governs it, whether erasure may reach it, and whether
//! the field is also a secret.
//!
//! The platform ships no legal defaults (ADR-061 s6: what may be erased and what must be kept
//! is policy data on the field, owned by whoever declares the field), so every one of these is
//! supplied by the caller. Nothing here is inferred.

use crate::error::PiiError;

/// One step of a field path: an object member, or every element of an array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The object member with this name.
    Key(String),
    /// Every element of the array at this position; written `*`.
    All,
}

/// A parsed field path, such as `email`, `contact.email` or `items.*.note`.
///
/// A path is dot-separated. `*` walks every element of the array at that position, so one
/// declaration covers every child row while the row count and the row order stay untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath {
    text: String,
    steps: Vec<Step>,
}

impl FieldPath {
    /// Parses `text`.
    ///
    /// # Errors
    ///
    /// `journal_pii.spec_invalid` when the text is empty, has an empty segment (a leading,
    /// trailing or doubled dot) or a segment that is not a name.
    pub fn parse(text: &str) -> Result<Self, PiiError> {
        if text.is_empty() {
            return Err(PiiError::SpecInvalid("a field path is empty"));
        }
        let mut steps = Vec::new();
        for segment in text.split('.') {
            if segment.is_empty() {
                return Err(PiiError::SpecInvalid("a field path has an empty segment"));
            }
            if segment == "*" {
                steps.push(Step::All);
            } else if segment.contains('*') {
                return Err(PiiError::SpecInvalid(
                    "a field path segment is either a name or `*`, never both",
                ));
            } else {
                steps.push(Step::Key(segment.to_owned()));
            }
        }
        Ok(Self {
            text: text.to_owned(),
            steps,
        })
    }

    /// The steps, in order from the image root.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// The path as it was written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// True when the path walks no array, so it addresses exactly one position in an image.
    #[must_use]
    pub fn is_single(&self) -> bool {
        !self.steps.iter().any(|step| matches!(step, Step::All))
    }
}

/// One personal field of one entity.
///
/// Built with [`FieldSpec::new`] and then, when the field is not erasable or is also a secret,
/// with [`FieldSpec::with_erasable`] or [`FieldSpec::with_secret`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpec {
    path: FieldPath,
    subject_path: FieldPath,
    purpose: String,
    retention_class: String,
    erasable: bool,
    secret: bool,
}

impl FieldSpec {
    /// A personal field at `path`, whose data-subject id is the image value at `subject_path`,
    /// held for `purpose` under `retention_class`, erasable and not a secret.
    ///
    /// # Errors
    ///
    /// `journal_pii.spec_invalid` when a path does not parse, when `subject_path` walks an
    /// array (a subject is a record-level value in this version), or when `purpose` or
    /// `retention_class` is empty.
    pub fn new(
        path: &str,
        subject_path: &str,
        purpose: &str,
        retention_class: &str,
    ) -> Result<Self, PiiError> {
        let path = FieldPath::parse(path)?;
        let subject_path = FieldPath::parse(subject_path)?;
        if !subject_path.is_single() {
            return Err(PiiError::SpecInvalid(
                "a subject path addresses one record-level value and never walks an array",
            ));
        }
        if purpose.is_empty() {
            return Err(PiiError::SpecInvalid("a personal field has no purpose"));
        }
        if retention_class.is_empty() {
            return Err(PiiError::SpecInvalid(
                "a personal field has no retention class",
            ));
        }
        Ok(Self {
            path,
            subject_path,
            purpose: purpose.to_owned(),
            retention_class: retention_class.to_owned(),
            erasable: true,
            secret: false,
        })
    }

    /// Whether erasure may reach this field. `false` is a legal-retention field: it is sealed
    /// under the tenant journal key ([`crate::SubjectKeys::retained_key`]) and survives the
    /// destruction of the subject's key.
    #[must_use]
    pub fn with_erasable(mut self, erasable: bool) -> Self {
        self.erasable = erasable;
        self
    }

    /// Whether the field is also a secret. A secret is stored encrypted by `spot-secrets`
    /// already and the journal never decrypts one, so this crate passes it through untouched
    /// (ADR-061 s3).
    #[must_use]
    pub fn with_secret(mut self, secret: bool) -> Self {
        self.secret = secret;
        self
    }

    /// Where the value sits in the image.
    #[must_use]
    pub fn path(&self) -> &FieldPath {
        &self.path
    }

    /// Where the data-subject id sits in the image.
    #[must_use]
    pub fn subject_path(&self) -> &FieldPath {
        &self.subject_path
    }

    /// Why the value is held, echoed into the erasure placeholder.
    #[must_use]
    pub fn purpose(&self) -> &str {
        &self.purpose
    }

    /// The retention class the field's declarer assigned.
    #[must_use]
    pub fn retention_class(&self) -> &str {
        &self.retention_class
    }

    /// Whether erasure may reach this field.
    #[must_use]
    pub fn erasable(&self) -> bool {
        self.erasable
    }

    /// Whether this field is a secret, which this crate never touches.
    #[must_use]
    pub fn secret(&self) -> bool {
        self.secret
    }
}

/// The personal fields of one entity, as the entity's declarer describes them.
///
/// The entity name is part of every envelope's associated data, so an envelope cannot be moved
/// to a record of another entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonalFields {
    entity: String,
    fields: Vec<FieldSpec>,
}

impl PersonalFields {
    /// A description of `entity`'s personal fields, with none declared yet.
    ///
    /// # Errors
    ///
    /// `journal_pii.spec_invalid` when `entity` is empty.
    pub fn new(entity: &str) -> Result<Self, PiiError> {
        if entity.is_empty() {
            return Err(PiiError::SpecInvalid("the entity name is empty"));
        }
        Ok(Self {
            entity: entity.to_owned(),
            fields: Vec::new(),
        })
    }

    /// The same description with `field` added.
    #[must_use]
    pub fn with_field(mut self, field: FieldSpec) -> Self {
        self.fields.push(field);
        self
    }

    /// The entity these fields belong to.
    #[must_use]
    pub fn entity(&self) -> &str {
        &self.entity
    }

    /// The declared fields, in declaration order.
    #[must_use]
    pub fn fields(&self) -> &[FieldSpec] {
        &self.fields
    }

    /// Checks the description as a whole: a non-empty entity and no two fields at one path.
    ///
    /// # Errors
    ///
    /// `journal_pii.spec_invalid` when the entity is empty or two fields share a path (the
    /// second would overwrite the first envelope, losing a value).
    pub fn validate(&self) -> Result<(), PiiError> {
        if self.entity.is_empty() {
            return Err(PiiError::SpecInvalid("the entity name is empty"));
        }
        for (index, field) in self.fields.iter().enumerate() {
            for other in self.fields.iter().skip(index + 1) {
                if field.path() == other.path() {
                    return Err(PiiError::SpecInvalid("two personal fields share one path"));
                }
            }
        }
        Ok(())
    }
}

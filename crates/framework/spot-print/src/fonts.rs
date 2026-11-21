//! The fonts a document may use: bundled Noto plus tenant fonts, all from
//! bytes. Nothing is read from the system.

use typst::foundations::Bytes;
use typst::text::Font;

use crate::error::PrintError;

const BUNDLED: [&[u8]; 4] = [
    include_bytes!("../fonts/NotoSans-Regular.ttf"),
    include_bytes!("../fonts/NotoSans-Bold.ttf"),
    include_bytes!("../fonts/NotoNaskhArabic-Regular.ttf"),
    include_bytes!("../fonts/NotoNaskhArabic-Bold.ttf"),
];

/// The set of fonts available to documents. Parsed once; cloning is cheap.
#[derive(Debug, Clone)]
pub struct FontSet {
    fonts: Vec<Font>,
}

impl FontSet {
    /// Noto Sans and Noto Naskh Arabic (Regular and Bold), compiled into the
    /// binary under the SIL Open Font Licence (see `fonts/` and
    /// `THIRD_PARTY.md`). Family names: `"Noto Sans"`, `"Noto Naskh Arabic"`.
    #[must_use]
    pub fn bundled() -> Self {
        Self {
            fonts: parse_all(BUNDLED.iter().map(|bytes| bytes.to_vec())),
        }
    }

    /// An empty set; only useful as a base for [`FontSet::with`].
    #[must_use]
    pub fn empty() -> Self {
        Self { fonts: Vec::new() }
    }

    /// Adds tenant fonts (TTF/OTF/TTC bytes).
    ///
    /// # Errors
    /// [`PrintError::Font`] when an entry holds no parseable font face.
    pub fn with<I>(mut self, extra: I) -> Result<Self, PrintError>
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        for (index, data) in extra.into_iter().enumerate() {
            let faces = parse_all(std::iter::once(data));
            if faces.is_empty() {
                return Err(PrintError::Font {
                    message: format!("extra font #{index} contains no usable font face"),
                });
            }
            self.fonts.extend(faces);
        }
        Ok(self)
    }

    /// The number of font faces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fonts.len()
    }

    /// Whether the set holds no faces.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }

    pub(crate) fn faces(&self) -> &[Font] {
        &self.fonts
    }
}

fn parse_all(blobs: impl Iterator<Item = Vec<u8>>) -> Vec<Font> {
    blobs
        .flat_map(|data| Font::iter(Bytes::new(data)))
        .collect()
}

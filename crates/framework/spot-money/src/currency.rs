//! Currencies, units of measure and the currency table. The table is **data**: this crate has no
//! built-in list of currencies or minor-unit exponents.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{MoneyError, Result};
use crate::exact::MAX_SCALE;

/// A currency code: three uppercase ASCII letters (`USD`, `JPY`). Validated at construction; what
/// the code means (its minor-unit exponent) is the [`CurrencyTable`]'s business.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Currency([u8; 3]);

impl Currency {
    /// Validates a currency code.
    pub fn new(code: &str) -> Result<Currency> {
        match code.as_bytes() {
            [a, b, c] if [a, b, c].iter().all(|x| x.is_ascii_uppercase()) => {
                Ok(Currency([*a, *b, *c]))
            }
            _ => Err(MoneyError::BadCurrency(code.to_owned())),
        }
    }

    /// The code as text.
    pub fn code(&self) -> &str {
        // The bytes are ASCII uppercase letters by construction.
        std::str::from_utf8(&self.0).unwrap_or("???")
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl Serialize for Currency {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let code = String::deserialize(deserializer)?;
        Currency::new(&code).map_err(serde::de::Error::custom)
    }
}

/// A unit of measure (`kg`, `m2`, `box_of_12`): 1 to 32 characters without whitespace or control
/// characters. Units are opaque here; conversion between units is not this crate's job.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Uom(String);

impl Uom {
    /// Longest unit in characters.
    pub const MAX_LEN: usize = 32;

    /// Validates a unit.
    pub fn new(unit: &str) -> Result<Uom> {
        let ok = !unit.is_empty()
            && unit.chars().count() <= Self::MAX_LEN
            && unit.chars().all(|c| !c.is_whitespace() && !c.is_control());
        if ok {
            Ok(Uom(unit.to_owned()))
        } else {
            Err(MoneyError::BadUom(unit.to_owned()))
        }
    }

    /// The unit as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Uom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Uom {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let unit = String::deserialize(deserializer)?;
        Uom::new(&unit).map_err(serde::de::Error::custom)
    }
}

/// Which currencies exist and how many minor-unit digits each has, supplied by the caller (from
/// configuration or an app), never by this crate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CurrencyTable {
    exponents: BTreeMap<Currency, u32>,
}

impl CurrencyTable {
    /// Builds a table from `(currency, exponent)` entries. An exponent is the number of
    /// fractional digits of the minor unit (2 for cents, 0 for yen) and at most 28; a currency
    /// may appear once.
    pub fn new(entries: impl IntoIterator<Item = (Currency, u32)>) -> Result<CurrencyTable> {
        let mut exponents = BTreeMap::new();
        for (currency, exponent) in entries {
            if exponent > MAX_SCALE {
                return Err(MoneyError::BadScale(exponent));
            }
            if exponents.insert(currency, exponent).is_some() {
                return Err(MoneyError::DuplicateCurrency(currency.code().to_owned()));
            }
        }
        Ok(CurrencyTable { exponents })
    }

    /// The minor-unit exponent of `currency`.
    pub fn exponent(&self, currency: &Currency) -> Result<u32> {
        self.exponents
            .get(currency)
            .copied()
            .ok_or_else(|| MoneyError::UnknownCurrency(currency.code().to_owned()))
    }

    /// Whether the table has `currency`.
    pub fn contains(&self, currency: &Currency) -> bool {
        self.exponents.contains_key(currency)
    }

    /// The currencies in the table, in code order.
    pub fn currencies(&self) -> impl Iterator<Item = &Currency> {
        self.exponents.keys()
    }
}

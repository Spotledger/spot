//! The personal-field transform as the shipper's [`Protector`].

use spot_journal_protect::ProtectTransform;

use crate::error::ShipFault;
use crate::ports::Protector;

impl Protector for ProtectTransform {
    fn protect(&self, line: &[u8]) -> Result<Vec<u8>, ShipFault> {
        self.transform_line(line)
            .map_err(|error| ShipFault::Protect { code: error.code() })
    }
}

/// A protector that writes every line as it is. Used when a site declares no personal fields;
/// the readiness line of the host says so. With any personal field declared, a real transform
/// must be provided instead (the host refuses to ship without one, see `journal_privacy`).
#[derive(Clone, Copy, Debug, Default)]
pub struct PassThrough;

impl Protector for PassThrough {
    fn protect(&self, line: &[u8]) -> Result<Vec<u8>, ShipFault> {
        Ok(line.to_vec())
    }
}

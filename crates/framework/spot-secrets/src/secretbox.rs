use crate::blindindex::BlindIndex;
use crate::crypto::{self, KEY_LEN};
use crate::error::SecretError;
use crate::provider::KeyProvider;
use crate::sealed::Sealed;
use crate::secret::SecretString;
use zeroize::Zeroizing;

/// Seals and opens secret values. This is the API other crates use.
///
/// `aad` binds a value to where it is stored, for example
/// `site=acme;doctype=Email Account;field=password;name=Sales`. A sealed value copied to
/// another field fails to open.
pub struct SecretBox<P: KeyProvider> {
    provider: P,
}

impl<P: KeyProvider> SecretBox<P> {
    /// A box using `provider` for key wrapping.
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    /// The key provider.
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Encrypts `secret` under a fresh data key wrapped by the current KEK.
    pub async fn seal(&self, secret: &SecretString, aad: &[u8]) -> Result<Sealed, SecretError> {
        let data_key = crypto::random_key()?;
        let body = crypto::encrypt(&data_key, aad, secret.expose().as_bytes())?;
        let wrapped = self.provider.wrap(data_key.as_slice(), aad).await?;
        check_ref_fits(&wrapped.key.id, wrapped.bytes.len())?;
        Ok(Sealed { wrapped, body })
    }

    /// Decrypts `sealed`. Fails with `secrets.tampered` on any alteration or `aad` mismatch.
    pub async fn open(&self, sealed: &Sealed, aad: &[u8]) -> Result<SecretString, SecretError> {
        let data_key = self.unwrap_data_key(sealed, aad).await?;
        let plain = crypto::decrypt(&data_key, aad, &sealed.body)?;
        let text = String::from_utf8(plain.to_vec()).map_err(|_| SecretError::Tampered)?;
        Ok(SecretString::new(text))
    }

    /// True when `sealed` is not under the current KEK version.
    pub fn needs_rewrap(&self, sealed: &Sealed) -> bool {
        *sealed.key_ref() != self.provider.current()
    }

    /// The blind index of `value` in `scope` (`"<doctype>.<field>"`), under the current KEK
    /// version, for a `secret` field that declares `lookup = true`.
    ///
    /// Nothing is sealed or stored here: the result is what goes in the field's `<field>__bidx`
    /// column, and looking a secret up means deriving this again from the value being looked
    /// for and comparing the whole 33 bytes. The index key comes from the provider's KEK
    /// ([`KeyProvider::index_key`]) and does not outlive this call.
    ///
    /// A key version above 255 cannot be written in one byte, so it is refused instead of
    /// wrapped silently: a wrapped byte would make two versions' indexes indistinguishable.
    pub async fn blind_index(&self, scope: &str, value: &[u8]) -> Result<BlindIndex, SecretError> {
        crate::blindindex::check_scope(scope)?;
        let current = self.provider.current();
        let version = u8::try_from(current.version).map_err(|_| {
            SecretError::IndexKey(format!(
                "key version {} does not fit the blind index format",
                current.version
            ))
        })?;
        if version == 0 {
            return Err(SecretError::IndexKey("key versions start at 1".into()));
        }
        let index_key = self.provider.index_key(&current).await?;
        BlindIndex::compute(index_key.expose(), version, scope, value)
    }

    /// True when `index` was derived under a KEK version other than the current one, so it must
    /// be recomputed after a rewrap (the version byte is the only part of it that changes by
    /// version; the MAC itself changes with the key).
    pub fn needs_reindex(&self, index: &BlindIndex) -> bool {
        u32::from(index.version()) != self.provider.current().version
    }

    /// Re-wraps the data key under the current KEK **without decrypting the value**, so
    /// rotation needs no downtime: old and new versions both open until every value has been
    /// rewrapped.
    pub async fn rewrap(&self, sealed: &Sealed, aad: &[u8]) -> Result<Sealed, SecretError> {
        let data_key = self.unwrap_data_key(sealed, aad).await?;
        let wrapped = self.provider.wrap(data_key.as_slice(), aad).await?;
        check_ref_fits(&wrapped.key.id, wrapped.bytes.len())?;
        Ok(Sealed {
            wrapped,
            body: sealed.body.clone(),
        })
    }

    async fn unwrap_data_key(
        &self,
        sealed: &Sealed,
        aad: &[u8],
    ) -> Result<Zeroizing<[u8; KEY_LEN]>, SecretError> {
        let bytes = self
            .provider
            .unwrap(sealed.key_ref(), &sealed.wrapped, aad)
            .await?;
        let key: [u8; KEY_LEN] = bytes
            .expose()
            .try_into()
            .map_err(|_| SecretError::Malformed("data key has the wrong length"))?;
        Ok(Zeroizing::new(key))
    }
}

/// The binary format stores the id length in one byte and the wrapped key length in two.
fn check_ref_fits(id: &str, wrapped_len: usize) -> Result<(), SecretError> {
    if id.len() > usize::from(u8::MAX) || wrapped_len > usize::from(u16::MAX) {
        return Err(SecretError::Provider(
            "key id or wrapped key too long for the sealed format".into(),
        ));
    }
    Ok(())
}

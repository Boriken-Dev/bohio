use super::{Signature, verify_digest};
use crate::PublicKey;
use core::marker::PhantomData;
use digest::{Digest, FixedOutputReset, Update};
use signature::{DigestVerifier, Verifier, hazmat::PrehashVerifier};

#[cfg(feature = "encoding")]
use {
    crate::encoding::ID_RSASSA_PSS,
    const_oid::AssociatedOid,
    pkcs8::{Document, EncodePublicKey},
    spki::{AlgorithmIdentifierRef, AssociatedAlgorithmIdentifier, der::AnyRef},
};
#[cfg(feature = "serde")]
use {
    serdect::serde::{Deserialize, Serialize, de, ser},
    spki::DecodePublicKey,
};

/// Verifying key for checking the validity of RSASSA-PSS signatures as
/// described in [RFC8017 § 8.1].
///
/// [RFC8017 § 8.1]: https://datatracker.ietf.org/doc/html/rfc8017#section-8.1
#[derive(Debug)]
pub struct VerifyingKey<D>
where
    D: Digest,
{
    pub(super) inner: PublicKey,
    pub(super) salt_len: Option<usize>,
    pub(super) phantom: PhantomData<D>,
}

impl<D> VerifyingKey<D>
where
    D: Digest,
{
    /// Create a new RSASSA-PSS verifying key.
    /// Digest output size is used as a salt length.
    #[must_use]
    pub fn new(key: PublicKey) -> Self {
        Self::new_with_salt_len(key, <D as Digest>::output_size())
    }

    /// Create a new RSASSA-PSS verifying key.
    #[must_use]
    pub fn new_with_salt_len(key: PublicKey, salt_len: usize) -> Self {
        Self {
            inner: key,
            salt_len: Some(salt_len),
            phantom: Default::default(),
        }
    }

    /// Create a new RSASSA-PSS verifying key.
    /// Attempts to automatically detect the salt length.
    #[must_use]
    pub fn new_with_auto_salt_len(key: PublicKey) -> Self {
        Self {
            inner: key,
            salt_len: None,
            phantom: Default::default(),
        }
    }

    /// Return specified salt length for this key
    #[must_use]
    pub fn salt_len(&self) -> Option<usize> {
        self.salt_len
    }
}

//
// `*Verifier` trait impls
//

impl<D> DigestVerifier<D, Signature> for VerifyingKey<D>
where
    D: Digest + FixedOutputReset + Update,
{
    fn verify_digest<F: Fn(&mut D) -> signature::Result<()>>(
        &self,
        f: F,
        signature: &Signature,
    ) -> signature::Result<()> {
        let mut digest = D::new();
        f(&mut digest)?;
        verify_digest::<D>(
            &self.inner,
            &digest.finalize(),
            &signature.inner,
            self.salt_len,
        )
        .map_err(Into::into)
    }
}

impl<D> PrehashVerifier<Signature> for VerifyingKey<D>
where
    D: Digest + FixedOutputReset,
{
    fn verify_prehash(&self, prehash: &[u8], signature: &Signature) -> signature::Result<()> {
        verify_digest::<D>(&self.inner, prehash, &signature.inner, self.salt_len)
            .map_err(Into::into)
    }
}

impl<D> Verifier<Signature> for VerifyingKey<D>
where
    D: Digest + FixedOutputReset,
{
    fn verify(&self, msg: &[u8], signature: &Signature) -> signature::Result<()> {
        verify_digest::<D>(
            &self.inner,
            &D::digest(msg),
            &signature.inner,
            self.salt_len,
        )
        .map_err(Into::into)
    }
}

//
// Other trait impls
//

impl<D> AsRef<PublicKey> for VerifyingKey<D>
where
    D: Digest,
{
    fn as_ref(&self) -> &PublicKey {
        &self.inner
    }
}

#[cfg(feature = "encoding")]
impl<D> AssociatedAlgorithmIdentifier for VerifyingKey<D>
where
    D: Digest,
{
    type Params = AnyRef<'static>;

    const ALGORITHM_IDENTIFIER: AlgorithmIdentifierRef<'static> = pkcs1::ALGORITHM_ID;
}

// Implemented manually so we don't have to bind D with Clone
impl<D> Clone for VerifyingKey<D>
where
    D: Digest,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            salt_len: self.salt_len,
            phantom: Default::default(),
        }
    }
}

#[cfg(feature = "encoding")]
impl<D> EncodePublicKey for VerifyingKey<D>
where
    D: Digest,
{
    fn to_public_key_der(&self) -> spki::Result<Document> {
        self.inner.to_public_key_der()
    }
}

impl<D> From<PublicKey> for VerifyingKey<D>
where
    D: Digest,
{
    fn from(key: PublicKey) -> Self {
        Self::new(key)
    }
}

impl<D> From<VerifyingKey<D>> for PublicKey
where
    D: Digest,
{
    fn from(key: VerifyingKey<D>) -> Self {
        key.inner
    }
}

#[cfg(feature = "encoding")]
impl<D> TryFrom<pkcs8::SubjectPublicKeyInfoRef<'_>> for VerifyingKey<D>
where
    D: Digest + AssociatedOid,
{
    type Error = spki::Error;

    fn try_from(spki: pkcs8::SubjectPublicKeyInfoRef<'_>) -> spki::Result<Self> {
        match spki.algorithm.oid {
            ID_RSASSA_PSS | pkcs1::ALGORITHM_OID => (),
            _ => {
                return Err(spki::Error::OidUnknown {
                    oid: spki.algorithm.oid,
                });
            }
        }

        PublicKey::try_from(spki).map(Self::new)
    }
}

impl<D> PartialEq for VerifyingKey<D>
where
    D: Digest,
{
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner && self.salt_len == other.salt_len
    }
}

#[cfg(feature = "serde")]
impl<D> Serialize for VerifyingKey<D>
where
    D: Digest,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let der = self.to_public_key_der().map_err(ser::Error::custom)?;
        serdect::slice::serialize_hex_lower_or_bin(&der, serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, D> Deserialize<'de> for VerifyingKey<D>
where
    D: Digest + AssociatedOid,
{
    fn deserialize<De>(deserializer: De) -> Result<Self, De::Error>
    where
        De: serde::Deserializer<'de>,
    {
        let der_bytes = serdect::slice::deserialize_hex_or_bin_vec(deserializer)?;
        Self::from_public_key_der(&der_bytes).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "serde")]
    fn test_serde() {
        use super::*;
        use pkcs8::DecodePublicKey;
        use serde_test::{Configure, Token, assert_tokens};
        use sha2::Sha256;

        let der = &[0x30u8,0x82,0x01,0x22,0x30,0x0d,0x06,0x09,0x2a,0x86,0x48,0x86,0xf7,0x0d,0x01,0x01,0x01,0x05,0x00,0x03,0x82,0x01,0x0f,0x00,0x30,0x82,0x01,0x0a,0x02,0x82,0x01,0x01,0x00,0xb6,0xc4,0x2c,0x51,0x5f,0x10,0xa6,0xaa,0xf2,0x82,0xc6,0x3e,0xdb,0xe2,0x42,0x43,0xa1,0x70,0xf3,0xfa,0x26,0x33,0xbd,0x48,0x33,0x63,0x7f,0x47,0xca,0x4f,0x6f,0x36,0xe0,0x3a,0x5d,0x29,0xef,0xc3,0x19,0x1a,0xc8,0x0f,0x39,0x0d,0x87,0x4b,0x39,0xe3,0x0f,0x41,0x4f,0xce,0xc1,0xfc,0xa0,0xed,0x81,0xe5,0x47,0xed,0xc2,0xcd,0x38,0x2c,0x76,0xf6,0x1c,0x90,0x18,0x97,0x3d,0xb9,0xfa,0x53,0x79,0x72,0xa7,0xc7,0x01,0xf6,0xb7,0x7e,0x09,0x82,0xdf,0xc1,0x5f,0xc0,0x19,0x27,0xee,0x5e,0x7c,0xd9,0x4b,0x4f,0x59,0x9f,0xf0,0x70,0x13,0xa7,0xc8,0x28,0x1b,0xdf,0x22,0xdc,0xbc,0x9a,0xd7,0xca,0xbb,0x7c,0x43,0x11,0xc9,0x82,0xf5,0x8e,0xdb,0x72,0x13,0xad,0x45,0x58,0xb3,0x32,0x26,0x6d,0x74,0x3a,0xed,0x81,0x92,0xd1,0x88,0x4c,0xad,0xb8,0xb1,0x47,0x39,0xa8,0xda,0xda,0x66,0xdc,0x97,0x08,0x06,0xd9,0xc7,0xac,0x45,0x0c,0xb1,0x3d,0x0d,0x7c,0x57,0x5f,0xb1,0x98,0x53,0x4f,0xc6,0x1b,0xc4,0x1b,0xc0,0xf0,0x57,0x4e,0x0e,0x01,0x30,0xc7,0xbb,0xbf,0xbd,0xfd,0xc9,0xf6,0xa6,0xe2,0xe3,0xe2,0xaf,0xf1,0xcb,0xea,0xc8,0x9b,0xa5,0x78,0x84,0x52,0x8d,0x55,0xcf,0xb0,0x83,0x27,0xa1,0xe8,0xc8,0x9f,0x4e,0x00,0x3c,0xf2,0x88,0x8e,0x93,0x32,0x41,0xd9,0xd6,0x95,0xbc,0xbb,0xac,0xdc,0x90,0xb4,0x4e,0x3e,0x09,0x5f,0xa3,0x70,0x58,0xea,0x25,0xb1,0x3f,0x5e,0x29,0x5c,0xbe,0xac,0x6d,0xe8,0x38,0xab,0x8c,0x50,0xaf,0x61,0xe2,0x98,0x97,0x5b,0x87,0x2f,0x02,0x03,0x01,0x00,0x01];
        let pub_key = PublicKey::from_public_key_der(der).unwrap();
        let encoded = include_str!("../../tests/examples/pkcs8/rsa2048-pub.hex").trim();
        let verifying_key = VerifyingKey::<Sha256>::new(pub_key);

        let tokens = [Token::Str(encoded)];

        assert_tokens(&verifying_key.readable(), &tokens);
    }
}

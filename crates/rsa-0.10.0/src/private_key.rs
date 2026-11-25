//! RSA private key type.

use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};

use crate::algorithms::generate::{generate_multi_prime_key_with_exp, generate_prime_with_rng};
use crate::algorithms::rsa::{
    compute_modulus, compute_private_exponent_carmicheal, compute_private_exponent_euler_totient,
    recover_primes, to_monty,
};
#[cfg(not(feature = "getrandom"))]
use crate::dummy_rng::DummyRng;
use crate::errors::{Error, Result};
use crate::traits::keys::{CrtValue, PrivateKeyParts, PublicKeyParts};
use crate::traits::{PaddingScheme, SignatureScheme};
use crate::{Generate, PublicKey, public_key};

use crypto_bigint::{
    BoxedUint, Choice, ConcatenatingMul, CtEq, CtLt, Integer, NonZero, Odd, Resize,
    modular::{BoxedMontyForm, BoxedMontyParams},
};
use rand_core::TryCryptoRng;
use zeroize::{Zeroize, ZeroizeOnDrop};

#[cfg(feature = "serde")]
use {
    pkcs8::{DecodePrivateKey, EncodePrivateKey},
    serdect::serde::{Deserialize, Serialize, de, ser},
};

/// Type we use for representing secret prime values.
pub(crate) type SecretPrime = crypto_bigint::modular::MontyParams<BoxedUint>;

/// Represents a whole RSA key, public and private parts.
#[derive(Clone)]
pub struct PrivateKey {
    /// Public components of the private key.
    pubkey_components: PublicKey,
    /// Private exponent.
    pub(crate) d: BoxedUint,
    /// Prime factors of N, contains >= 2 elements.
    pub(crate) primes: Vec<Odd<BoxedUint>>,
    /// Precomputed values to speed up private operations.
    pub(crate) precomputed: Option<PrecomputedValues>,
}

impl fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let precomputed = if self.precomputed.is_some() {
            "Some(...)"
        } else {
            "None"
        };
        f.debug_struct("PrivateKey")
            .field("pubkey_components", &self.pubkey_components)
            .field("d", &"...")
            .field("primes", &"&[...]")
            .field("precomputed", &precomputed)
            .finish()
    }
}

impl Eq for PrivateKey {}
impl PartialEq for PrivateKey {
    #[inline]
    fn eq(&self, other: &PrivateKey) -> bool {
        self.pubkey_components == other.pubkey_components
            && self.d == other.d
            && self.primes == other.primes
    }
}

impl AsRef<PublicKey> for PrivateKey {
    fn as_ref(&self) -> &PublicKey {
        &self.pubkey_components
    }
}

impl Hash for PrivateKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Domain separator for RSA private keys
        state.write(b"PrivateKey");
        Hash::hash(&self.pubkey_components, state);
    }
}

impl Drop for PrivateKey {
    fn drop(&mut self) {
        self.d.zeroize();
        self.primes.zeroize();
        self.precomputed.zeroize();
    }
}

impl ZeroizeOnDrop for PrivateKey {}

#[derive(Clone)]
pub(crate) struct PrecomputedValues {
    /// Montgomery params for `p`
    pub(crate) p: SecretPrime,
    /// Montgomery params for `q`
    pub(crate) q: SecretPrime,
    /// D mod (P-1)
    pub(crate) dp: BoxedUint,
    /// D mod (Q-1)
    pub(crate) dq: BoxedUint,
    /// Q^-1 mod P
    pub(crate) qinv: BoxedMontyForm,
}

impl ZeroizeOnDrop for PrecomputedValues {}

impl Zeroize for PrecomputedValues {
    fn zeroize(&mut self) {
        self.p.zeroize();
        self.q.zeroize();
        self.dp.zeroize();
        self.dq.zeroize();
    }
}

impl Drop for PrecomputedValues {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl PublicKeyParts for PrivateKey {
    fn n(&self) -> &NonZero<BoxedUint> {
        &self.pubkey_components.n
    }

    fn e(&self) -> &BoxedUint {
        &self.pubkey_components.e
    }

    fn n_params(&self) -> &BoxedMontyParams {
        &self.pubkey_components.n_params
    }
}

impl PrivateKey {
    /// Default modulus size in bits used by [`Generate`].
    ///
    /// A 3072-bit RSA modulus provides an estimated 128-bit security strength according to
    /// [NIST SP 800-57 Part 1 Rev. 5, Table 2](https://doi.org/10.6028/NIST.SP.800-57pt1r5).
    pub const DEFAULT_BITS: usize = 3072;

    /// Default exponent for RSA keys.
    const EXP: u64 = 65537;

    /// Generate a new RSA key pair with a modulus of the given bit size using the passed in `rng`.
    ///
    /// # Errors
    /// - If `bit_size` is lower than the minimum 1024-bits.
    /// - If the RNG fails, returns [`Error::Rng`].
    pub fn new<R: TryCryptoRng + ?Sized>(rng: &mut R, bit_size: usize) -> Result<Self> {
        Self::new_with_exp(rng, bit_size, Self::EXP.into())
    }

    /// Generate a new RSA key pair of the given bit size.
    ///
    /// <div class="warning">
    /// <b>⚠️Warning: Hazmat!</b>
    /// This version does not apply minimum key size checks, and as such may generate keys
    /// which are insecure!
    /// </div>
    #[cfg(feature = "hazmat")]
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn new_unchecked<R: TryCryptoRng + ?Sized>(rng: &mut R, bit_size: usize) -> Result<Self> {
        Self::new_with_exp_unchecked(rng, bit_size, Self::EXP.into())
    }

    /// Generate a new RSA key pair of the given bit size and the public exponent
    /// using the passed in `rng`.
    ///
    /// Unless you have specific needs, you should use [`PrivateKey::new`] instead.
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn new_with_exp<R: TryCryptoRng + ?Sized>(
        rng: &mut R,
        bit_size: usize,
        exp: BoxedUint,
    ) -> Result<PrivateKey> {
        if bit_size < PublicKey::MIN_SIZE {
            return Err(Error::ModulusTooSmall);
        }

        let components = generate_multi_prime_key_with_exp(rng, 2, bit_size, exp)?;
        PrivateKey::from_components(
            components.n.get(),
            components.e,
            components.d,
            components.primes,
        )
    }

    /// Generate a new RSA key pair of the given bit size and the public exponent
    /// using the passed in `rng`.
    ///
    /// Unless you have specific needs, you should use [`PrivateKey::new`] instead.
    ///
    /// <div class="warning">
    /// <b>⚠️Warning: Hazmat!</b>
    /// This version does not apply minimum key size checks, and as such may generate keys
    /// which are insecure!
    /// </div>
    #[cfg(feature = "hazmat")]
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn new_with_exp_unchecked<R: TryCryptoRng + ?Sized>(
        rng: &mut R,
        bit_size: usize,
        exp: BoxedUint,
    ) -> Result<PrivateKey> {
        let components = generate_multi_prime_key_with_exp(rng, 2, bit_size, exp)?;
        PrivateKey::from_components(
            components.n.get(),
            components.e,
            components.d,
            components.primes,
        )
    }

    /// Private helper function that constructs an RSA key pair from components
    /// WITHOUT performing any validation or precomputation.
    ///
    /// This is the shared implementation used by `from_components` and
    /// `from_components_with_large_exponent`.
    ///
    /// Callers are responsible for:
    /// 1. Validating the key
    /// 2. Calling `precompute()` after validation, propagating any error
    fn from_components_inner(
        n: BoxedUint,
        e: BoxedUint,
        d: BoxedUint,
        mut primes: Vec<BoxedUint>,
        wide_primes: bool,
    ) -> Result<PrivateKey> {
        let n = Odd::new(n).into_option().ok_or(Error::InvalidModulus)?;

        // The modulus may come in padded with zeros, shorten it
        // to ensure optimal performance of arithmetic operations.
        let n_bits = n.bit_width_vartime();
        let n = n.resize_unchecked(n_bits);

        // Use a public storage width even when the caller supplies a short or zero-padded `d`.
        // Check for truncation before resizing so an oversized exponent cannot become valid.
        let d = d
            .try_resize(n.bits_precision())
            .ok_or(Error::InvalidExponent)?;

        let n_params = BoxedMontyParams::new(n.clone());
        let n_c = NonZero::new(n.get())
            .into_option()
            .ok_or(Error::InvalidModulus)?;

        match primes.len() {
            0 => {
                // Recover `p` and `q` from `d`.
                // See method in Appendix C.2: https://nvlpubs.nist.gov/nistpubs/SpecialPublications/NIST.SP.800-56Br2.pdf
                let (p, q) = recover_primes(&n_c, &e, &d)?;
                primes.push(p);
                primes.push(q);
            }
            1 => return Err(Error::NprimesTooSmall),
            _ => {
                // Check that the product of primes matches the modulus.
                // This also ensures that `bit_precision` of each prime is <= that of the modulus,
                // and `bit_precision` of their product is >= that of the modulus.
                if primes
                    .iter()
                    .fold(BoxedUint::one(), |acc, p| acc.concatenating_mul(&p))
                    != n_c.as_ref()
                {
                    return Err(Error::InvalidModulus);
                }
            }
        }

        // The caller's public policy and the public modulus determine every prime's width.
        // Do not automatically select a wider policy based on the secret values of the primes.
        // Multiprime operations use the non-CRT path and retain modulus-width factors.
        let prime_bits = if wide_primes || primes.len() > 2 {
            n_bits
        } else {
            n_bits.div_ceil(2)
        };

        let primes = primes
            .into_iter()
            .map(|p| {
                if p.bit_width() > prime_bits {
                    return Err(Error::InvalidPrime);
                }

                Odd::new(p.resize_unchecked(prime_bits))
                    .into_option()
                    .ok_or(Error::InvalidPrime)
            })
            .collect::<Result<Vec<_>>>()?;

        let k = PrivateKey {
            pubkey_components: PublicKey {
                n: n_c,
                e,
                n_params,
            },
            d,
            primes,
            precomputed: None,
        };

        Ok(k)
    }

    /// Constructs an RSA key pair from individual components, accepting exponents outside
    /// the normal size bounds.
    ///
    /// See [`PrivateKey::from_components`] for an explanation on the parameters.
    ///
    /// <div class="warning">
    /// <b>⚠️ Warning: Hazmat!</b>
    ///
    /// This method accepts public exponents outside the standard bounds (2 ≤ e ≤ 2^33-1),
    /// but still performs full cryptographic validation to ensure the key is mathematically
    /// correct (i.e., verifies that de ≡ 1 mod λ(n)).
    ///
    /// **Note:** This method is dangerous as it can be used as a DOS vector if used with
    /// untrusted input: <https://www.imperialviolet.org/2012/03/17/rsados.html>
    ///
    /// This is intended for interoperating with systems that use non-standard exponents
    /// or loading legacy keys. Use [`PrivateKey::from_components`] for standard key
    /// construction.
    /// </div>
    #[cfg(feature = "hazmat")]
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn from_components_with_large_exponent(
        n: BoxedUint,
        e: BoxedUint,
        d: BoxedUint,
        primes: Vec<BoxedUint>,
    ) -> Result<PrivateKey> {
        let mut k = Self::from_components_inner(n, e, d, primes, false)?;

        // Validate everything except exponent size bounds.
        validate_skip_exponent_size(&k)?;
        k.precompute()?;

        Ok(k)
    }

    /// Constructs an RSA key pair from individual components:
    ///
    /// - `n`: RSA modulus
    /// - `e`: public exponent (i.e. encrypting exponent)
    /// - `d`: private exponent (i.e. decrypting exponent)
    /// - `primes`: distinct prime factors of `n`: typically two primes `p` and `q`. More than two
    ///   primes can be provided for multiprime RSA, however this is generally not recommended.
    ///   If no `primes` are provided, a prime factor recovery algorithm will be employed to attempt
    ///   to recover the factors (as described in [NIST SP 800-56B Revision 2] Appendix C.2).
    ///   This algorithm only works if there are just two prime factors `p` and `q` (as opposed to
    ///   multiprime), and `e` is between 2^16 and 2^256.
    ///
    /// For two-prime keys, each prime must fit in `ceil(bit_length(n) / 2)` bits. Both primes
    /// and their CRT exponents use this public-derived storage width, rounded up to whole limbs.
    /// Use [`Self::from_components_with_wide_primes`] to explicitly import unbalanced factors.
    /// Multiprime keys use modulus-width factors and the non-CRT private operation.
    ///
    ///  [NIST SP 800-56B Revision 2]: https://nvlpubs.nist.gov/nistpubs/SpecialPublications/NIST.SP.800-56Br2.pdf
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn from_components(
        n: BoxedUint,
        e: BoxedUint,
        d: BoxedUint,
        primes: Vec<BoxedUint>,
    ) -> Result<PrivateKey> {
        let mut k = Self::from_components_inner(n, e, d, primes, false)?;
        k.validate()?;
        k.precompute()?;
        Ok(k)
    }

    /// Constructs a key using public-modulus-width storage for every prime and CRT exponent.
    ///
    /// This explicitly supports unbalanced prime factors which do not fit the half-modulus bit
    /// bound of [`Self::from_components`]. It is slower than ordinary two-prime CRT arithmetic.
    /// The width policy must be chosen as public configuration, not by inspecting secret factor
    /// sizes or retrying a failed half-width import with this constructor.
    ///
    /// Encoded-key decoding uses the ordinary half-width policy. For unbalanced encoded keys,
    /// decode the components and pass them to this constructor explicitly.
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn from_components_with_wide_primes(
        n: BoxedUint,
        e: BoxedUint,
        d: BoxedUint,
        primes: Vec<BoxedUint>,
    ) -> Result<PrivateKey> {
        let mut k = Self::from_components_inner(n, e, d, primes, true)?;
        k.validate()?;
        k.precompute()?;
        Ok(k)
    }

    /// Constructs an RSA key pair from its two primes p and q.
    ///
    /// The half-modulus factor bound of [`Self::from_components`] applies.
    ///
    /// This will rebuild the private exponent and the modulus.
    ///
    /// Private exponent will be rebuilt using the method defined in
    /// [NIST 800-56B Section 6.2.1](https://nvlpubs.nist.gov/nistpubs/SpecialPublications/NIST.SP.800-56Br2.pdf#page=47).
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn from_p_q(p: BoxedUint, q: BoxedUint, public_exponent: BoxedUint) -> Result<PrivateKey> {
        // RSA factors must be odd and greater than one. Check before deriving n or d.
        let two = BoxedUint::from(2u64);
        if (p.ct_lt(&two) | q.ct_lt(&two) | !p.is_odd() | !q.is_odd()).to_bool() {
            return Err(Error::InvalidPrime);
        }

        let d = compute_private_exponent_carmicheal(&p, &q, &public_exponent)?;
        let primes = vec![p, q];
        let n = compute_modulus(&primes);

        Self::from_components(n.get(), public_exponent, d, primes)
    }

    /// Constructs an RSA key pair from its primes.
    ///
    /// The factor-width policy of [`Self::from_components`] applies.
    ///
    /// This will rebuild the private exponent and the modulus.
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn from_primes(primes: Vec<BoxedUint>, public_exponent: BoxedUint) -> Result<PrivateKey> {
        if primes.len() < 2 {
            return Err(Error::NprimesTooSmall);
        }

        // RSA factors must be odd and greater than one. Check before deriving n or d.
        let two = BoxedUint::from(2u64);
        let invalid_primes = primes.iter().fold(Choice::FALSE, |invalid, p| {
            invalid | p.ct_lt(&two) | !p.is_odd()
        });

        if invalid_primes.to_bool() {
            return Err(Error::InvalidPrime);
        }

        let n = compute_modulus(&primes);
        let d = compute_private_exponent_euler_totient(&primes, &public_exponent)?;

        Self::from_components(n.get(), public_exponent, d, primes)
    }

    /// Get the public key from the private key.
    ///
    /// Specific alternative to [`AsRef::as_ref`].
    #[must_use]
    pub fn as_public_key(&self) -> &PublicKey {
        &self.pubkey_components
    }

    /// Get the public key from the private key, cloning `n` and `e`.
    ///
    /// Generally this is not needed since `PrivateKey` implements the `PublicKey` trait,
    /// but it can occasionally be useful to discard the private information entirely.
    #[must_use]
    pub fn to_public_key(&self) -> PublicKey {
        self.pubkey_components.clone()
    }

    /// Performs some calculations to speed up private key operations.
    ///
    /// # Errors
    /// Returns [`Error::InvalidPrime`] if either of the first two factors is one, or if they
    /// are not coprime and the CRT coefficient cannot be computed.
    pub fn precompute(&mut self) -> Result<()> {
        if self.precomputed.is_some() {
            return Ok(());
        }

        let d = &self.d;
        let p = self.primes[0].clone();
        let q = self.primes[1].clone();

        let p_params = BoxedMontyParams::new(p.clone());
        let q_params = BoxedMontyParams::new(q.clone());

        let x = NonZero::new(p.wrapping_sub(BoxedUint::one()))
            .into_option()
            .ok_or(Error::InvalidPrime)?;

        let dp = d.rem(&x);

        let x = NonZero::new(q.wrapping_sub(BoxedUint::one()))
            .into_option()
            .ok_or(Error::InvalidPrime)?;
        let dq = d.rem(&x);

        let q_mod_p = to_monty(q.as_ref(), &p_params);
        let qinv = q_mod_p.invert().into_option().ok_or(Error::InvalidPrime)?;

        debug_assert_eq!(dp.bits_precision(), p.bits_precision());
        debug_assert_eq!(dq.bits_precision(), q.bits_precision());
        debug_assert_eq!(qinv.bits_precision(), p.bits_precision());
        debug_assert_eq!(p_params.bits_precision(), p.bits_precision());
        debug_assert_eq!(q_params.bits_precision(), q.bits_precision());

        // TODO(tarcieri): zeroize intermediate parameters
        let p = p_params.as_ref().clone();
        let q = q_params.as_ref().clone();
        self.precomputed = Some(PrecomputedValues { p, q, dp, dq, qinv });
        Ok(())
    }

    /// Clears precomputed values by setting to None
    pub fn clear_precomputed(&mut self) {
        self.precomputed = None;
    }

    /// Compute CRT coefficient: `(1/q) mod p`.
    #[must_use]
    pub fn crt_coefficient(&self) -> Option<BoxedUint> {
        let p = &self.primes[0];
        let q = &self.primes[1];
        Option::from(q.invert_odd_mod(p))
    }

    /// Performs basic sanity checks on the key.
    /// Returns `Ok(())` if everything is good, otherwise an appropriate error.
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn validate(&self) -> Result<()> {
        public_key::check_public(self)?;
        validate_private_key_parts(self)?;
        Ok(())
    }

    /// Decrypt the given message.
    ///
    /// With the `getrandom` feature (enabled by default), uses the operating system RNG for
    /// RSA blinding. Without this feature, decryption is unblinded; use [`Self::decrypt_blinded`]
    /// to supply an RNG explicitly.
    ///
    /// [`Pkcs1v15Encrypt`][crate::Pkcs1v15Encrypt] uses implicit rejection: invalid padding returns
    /// deterministic synthetic plaintext, not an error. The enclosing protocol must authenticate
    /// the returned message.
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn decrypt<P: PaddingScheme>(&self, padding: P, ciphertext: &[u8]) -> Result<Vec<u8>> {
        #[cfg(feature = "getrandom")]
        {
            padding.decrypt(
                Some(&mut crypto_common::getrandom::SysRng),
                self,
                ciphertext,
            )
        }
        #[cfg(not(feature = "getrandom"))]
        {
            padding.decrypt(Option::<&mut DummyRng>::None, self, ciphertext)
        }
    }

    /// Decrypt the given message.
    ///
    /// Uses `rng` to blind the decryption process.
    ///
    /// [`Pkcs1v15Encrypt`][crate::Pkcs1v15Encrypt] has the same implicit-rejection
    /// behavior as [`Self::decrypt`], independently of the blinding randomness.
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn decrypt_blinded<R: TryCryptoRng + ?Sized, P: PaddingScheme>(
        &self,
        rng: &mut R,
        padding: P,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>> {
        padding.decrypt(Some(rng), self, ciphertext)
    }

    /// Sign the given digest.
    ///
    /// With the `getrandom` feature (enabled by default), supplies the operating system RNG for
    /// RSA blinding and any randomness required by the signature scheme. Without this feature,
    /// use [`Self::sign_with_rng`] to supply an RNG explicitly.
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn sign<S: SignatureScheme>(&self, padding: S, digest_in: &[u8]) -> Result<Vec<u8>> {
        #[cfg(feature = "getrandom")]
        {
            padding.sign(Some(&mut crypto_common::getrandom::SysRng), self, digest_in)
        }
        #[cfg(not(feature = "getrandom"))]
        {
            padding.sign(Option::<&mut DummyRng>::None, self, digest_in)
        }
    }

    /// Sign the given digest using the provided `rng`, which is used in the
    /// following ways depending on the [`SignatureScheme`]:
    ///
    /// - [`Pkcs1v15Sign`][`crate::Pkcs1v15Sign`] padding: uses the RNG
    ///   to mask the private key operation with random blinding, which helps
    ///   mitigate sidechannel attacks.
    /// - [`Pss`][`crate::Pss`] always requires randomness. Use
    ///   [`Pss::new`][`crate::Pss::new`] for a standard RSASSA-PSS signature with
    ///   RSA base blinding enabled by default.
    #[allow(clippy::missing_errors_doc, reason = "TODO")]
    pub fn sign_with_rng<R: TryCryptoRng + ?Sized, S: SignatureScheme>(
        &self,
        rng: &mut R,
        padding: S,
        digest_in: &[u8],
    ) -> Result<Vec<u8>> {
        padding.sign(Some(rng), self, digest_in)
    }
}

/// Generate a key with a [`Self::DEFAULT_BITS`]-bit modulus and public exponent 65537.
impl Generate for PrivateKey {
    fn try_generate_from_rng<R: TryCryptoRng + ?Sized>(
        rng: &mut R,
    ) -> core::result::Result<Self, R::Error> {
        let Ok(prime_bits) = u32::try_from(Self::DEFAULT_BITS.div_euclid(2)) else {
            unreachable!("default prime size fits in u32");
        };

        loop {
            // Setting the top two bits of both primes ensures a full-size modulus.
            let p = generate_prime_with_rng(rng, prime_bits)?;
            let q = generate_prime_with_rng(rng, prime_bits)?;

            match Self::from_primes(vec![p, q], Self::EXP.into()) {
                Ok(key) => return Ok(key),
                // Retry if the primes coincide or the public exponent is not invertible.
                Err(Error::InvalidPrime) => continue,
                Err(err) => unreachable!("invalid generated RSA key: {err}"),
            }
        }
    }
}

impl PrivateKeyParts for PrivateKey {
    fn d(&self) -> &BoxedUint {
        &self.d
    }

    fn primes(&self) -> &[Odd<BoxedUint>] {
        &self.primes
    }

    fn dp(&self) -> Option<&BoxedUint> {
        self.precomputed.as_ref().map(|p| &p.dp)
    }

    fn dq(&self) -> Option<&BoxedUint> {
        self.precomputed.as_ref().map(|p| &p.dq)
    }

    fn qinv(&self) -> Option<&BoxedMontyForm> {
        self.precomputed.as_ref().map(|p| &p.qinv)
    }

    fn crt_values(&self) -> Option<&[CrtValue]> {
        None
    }

    fn p(&self) -> Option<&SecretPrime> {
        self.precomputed.as_ref().map(|p| &p.p)
    }

    fn q(&self) -> Option<&SecretPrime> {
        self.precomputed.as_ref().map(|p| &p.q)
    }
}

/// Helper function that validates the private key structure and cryptographic correctness.
///
/// This performs the structural and mathematical validation checks that are common to both
/// `validate()` and `validate_skip_exponent_size()`.
fn validate_private_key_parts(key: &PrivateKey) -> Result<()> {
    // RFC8017 §3.2 requires `d < n`, even if a larger exponent satisfies the congruences below.
    // Preserve the supplied d rather than reducing it.
    if key.d >= key.n().as_ref() {
        return Err(Error::InvalidExponent);
    }

    // Check that Πprimes == n.
    let mut m = BoxedUint::one_with_precision(key.pubkey_components.n.bits_precision());
    let two = BoxedUint::from(2u64);
    let mut invalid_primes = Choice::FALSE;
    for (i, prime) in key.primes.iter().enumerate() {
        // Any primes <= 1 will cause divide-by-zero panics later.
        invalid_primes |= prime.as_ref().ct_lt(&two);

        // RFC 8017 §3.2 requires distinct prime factors, including for multiprime RSA.
        for other in &key.primes[..i] {
            invalid_primes |= prime.ct_eq(other);
        }
        m = m.wrapping_mul(prime.as_ref());
    }

    // Reveal only whether the factors are invalid, not which factor or pair failed.
    if invalid_primes.to_bool() {
        return Err(Error::InvalidPrime);
    }

    if m.ct_ne(key.pubkey_components.n.as_ref()).to_bool() {
        return Err(Error::InvalidModulus);
    }

    // Check that de ≡ 1 mod p-1, for each prime.
    // This implies that e is coprime to each p-1 as e has a multiplicative
    // inverse. Therefore e is coprime to lcm(p-1,q-1,r-1,...) =
    // exponent(ℤ/nℤ). It also implies that a^de ≡ a mod p as a^(p-1) ≡ 1
    // mod p. Thus a^de ≡ a mod n for all a coprime to n, as required.
    let de = key.d.concatenating_mul(&key.pubkey_components.e);

    for prime in &key.primes {
        let x = NonZero::new(prime.wrapping_sub(BoxedUint::one())).unwrap();
        let congruence = de.rem(&x);
        if !congruence.is_one().to_bool() {
            return Err(Error::InvalidExponent);
        }
    }

    Ok(())
}

/// Validate the private key structure and cryptographic correctness,
/// skipping only the exponent size bounds checks.
///
/// This performs all the same checks as `PrivateKey::validate()` except
/// it doesn't verify that the exponent is within the standard bounds.
#[cfg(feature = "hazmat")]
fn validate_skip_exponent_size(key: &PrivateKey) -> Result<()> {
    // Check public key properties (without exponent size checks)
    public_key::check_public_skip_exponent_size(
        key.pubkey_components.n.as_ref(),
        &key.pubkey_components.e,
    )?;

    // Perform common private key validation
    validate_private_key_parts(key)?;

    Ok(())
}

#[cfg(feature = "serde")]
impl Serialize for PrivateKey {
    fn serialize<S>(&self, serializer: S) -> core::prelude::v1::Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        let der = self.to_pkcs8_der().map_err(ser::Error::custom)?;
        serdect::slice::serialize_hex_lower_or_bin(&der.as_bytes(), serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for PrivateKey {
    fn deserialize<D>(deserializer: D) -> core::prelude::v1::Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let der_bytes = serdect::slice::deserialize_hex_or_bin_vec(deserializer)?;
        Self::from_pkcs8_der(&der_bytes).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::rsa::{rsa_decrypt_and_check, rsa_encrypt};
    use crate::traits::{PrivateKeyParts, PublicKeyParts};

    use hex_literal::hex;
    use rand::rngs::ChaCha8Rng;
    use rand_core::SeedableRng;

    #[cfg(feature = "encoding")]
    use pkcs8::DecodePrivateKey;

    #[test]
    fn non_coprime_factors_precompute_error() {
        // Distinct odd composite factors can pass validation: 3 * 19 = 1 mod 8 and mod 14.
        // However, neither factor is invertible modulo the other, so precomputation fails.
        for (p, q) in [(9u64, 15u64), (15, 9)] {
            let n = BoxedUint::from(p * q);
            let e = BoxedUint::from(3u64);
            let d = BoxedUint::from(19u64);
            let primes = vec![BoxedUint::from(p), BoxedUint::from(q)];
            let mut key = PrivateKey::from_components_inner(
                n.clone(),
                e.clone(),
                d.clone(),
                primes.clone(),
                false,
            )
            .unwrap();
            key.validate().unwrap();
            assert_eq!(key.precompute(), Err(Error::InvalidPrime));
            assert!(key.precomputed.is_none());

            assert_eq!(
                PrivateKey::from_components(n.clone(), e.clone(), d.clone(), primes.clone()),
                Err(Error::InvalidPrime)
            );
            assert_eq!(
                PrivateKey::from_components_with_wide_primes(
                    n.clone(),
                    e.clone(),
                    d.clone(),
                    primes.clone(),
                ),
                Err(Error::InvalidPrime)
            );
            #[cfg(feature = "hazmat")]
            assert_eq!(
                PrivateKey::from_components_with_large_exponent(n, e.clone(), d, primes.clone()),
                Err(Error::InvalidPrime)
            );
            assert_eq!(
                PrivateKey::from_p_q(primes[0].clone(), primes[1].clone(), e.clone()),
                Err(Error::InvalidPrime)
            );
            assert_eq!(PrivateKey::from_primes(primes, e), Err(Error::InvalidPrime));
        }
    }

    #[test]
    fn repeated_prime_factors() {
        // Each d satisfies e*d = 1 mod (p-1) for every supplied factor, so the exponent
        // checks alone cannot reject these keys. Include duplicates in every pair of a
        // three-factor key and a duplicate which does not involve either CRT factor.
        for (factors, d) in [
            (&[61u64, 61][..], 53u64),
            (&[61, 61, 53][..], 413),
            (&[61, 53, 61][..], 413),
            (&[61, 53, 53][..], 413),
            (&[61, 53, 47, 47][..], 10553),
        ] {
            // Different input precisions must not disguise equal factors.
            let primes: Vec<_> = factors
                .iter()
                .zip([64, 128, 192, 256])
                .map(|(&p, bits)| BoxedUint::from(p).resize_unchecked(bits))
                .collect();
            let n = compute_modulus(&primes).get();
            let e = BoxedUint::from(17u64);
            let d = BoxedUint::from(d);
            let key = PrivateKey::from_components_inner(
                n.clone(),
                e.clone(),
                d.clone(),
                primes.clone(),
                false,
            )
            .unwrap();
            assert_eq!(key.validate(), Err(Error::InvalidPrime));
            assert_eq!(
                PrivateKey::from_components(n.clone(), e.clone(), d.clone(), primes.clone()),
                Err(Error::InvalidPrime)
            );
            assert_eq!(
                PrivateKey::from_components_with_wide_primes(
                    n.clone(),
                    e.clone(),
                    d.clone(),
                    primes.clone(),
                ),
                Err(Error::InvalidPrime)
            );
            #[cfg(feature = "hazmat")]
            assert_eq!(
                PrivateKey::from_components_with_large_exponent(n, e.clone(), d, primes.clone()),
                Err(Error::InvalidPrime)
            );
            if primes.len() == 2 {
                assert_eq!(
                    PrivateKey::from_p_q(primes[0].clone(), primes[1].clone(), e.clone()),
                    Err(Error::InvalidPrime)
                );
            }
            assert_eq!(PrivateKey::from_primes(primes, e), Err(Error::InvalidPrime));
        }
    }

    #[test]
    fn invalid_factors_before_exponent_derivation() {
        for p in [0u64, 1, 2, 4] {
            for q in [0u64, 1, 2, 4, 61] {
                for (p, q) in [(p, q), (q, p)] {
                    let p = BoxedUint::from(p);
                    let q = BoxedUint::from(q);
                    let e = BoxedUint::from(17u64);
                    assert_eq!(
                        PrivateKey::from_p_q(p.clone(), q.clone(), e.clone()),
                        Err(Error::InvalidPrime)
                    );
                    assert_eq!(
                        PrivateKey::from_primes(vec![p, q], e),
                        Err(Error::InvalidPrime)
                    );
                }
            }

            // Check every position in a multiprime key, including beyond the first two factors.
            for i in 0..3 {
                let mut primes = vec![61u64.into(), 53u64.into(), 47u64.into()];
                primes[i] = BoxedUint::from(p);
                assert_eq!(
                    PrivateKey::from_primes(primes, BoxedUint::from(17u64)),
                    Err(Error::InvalidPrime)
                );
            }
        }
    }

    #[test]
    fn key_generation_rejects_repeated_factors() {
        use core::{convert::Infallible, result::Result};
        use rand_core::{TryCryptoRng, TryRng};

        // Repeating entropy produces the same prime twice. Bound the entropy budget so retrying
        // forever on this broken RNG fails the test instead of hanging it.
        struct RepeatingRng(usize);

        impl TryRng for RepeatingRng {
            type Error = Infallible;

            fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
                let mut bytes = [0; 4];
                self.try_fill_bytes(&mut bytes)?;
                Ok(u32::from_le_bytes(bytes))
            }

            fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
                let mut bytes = [0; 8];
                self.try_fill_bytes(&mut bytes)?;
                Ok(u64::from_le_bytes(bytes))
            }

            fn try_fill_bytes(&mut self, bytes: &mut [u8]) -> Result<(), Self::Error> {
                self.0 = self
                    .0
                    .checked_sub(bytes.len())
                    .expect("key generation retried");
                bytes.fill(0xaa);
                Ok(())
            }
        }

        impl TryCryptoRng for RepeatingRng {}

        assert_eq!(
            PrivateKey::new(&mut RepeatingRng(128), 1024),
            Err(Error::InvalidPrime)
        );
    }

    #[test]
    fn private_exponent_range_and_identity() {
        // For `e=17`, these exponents are equivalent modulo `lambda(n)=780`.
        for (e, d) in [(17u64, 413u64), (17, 2753), (17, 5873), (497, 3233)] {
            let result = PrivateKey::from_components(
                BoxedUint::from(3233u64),
                BoxedUint::from(e),
                BoxedUint::from(d),
                vec![BoxedUint::from(61u64), BoxedUint::from(53u64)],
            );
            if d < 3233 {
                assert_eq!(result.unwrap().d(), &BoxedUint::from(d));
            } else {
                assert_eq!(result, Err(Error::InvalidExponent));
            }
        }
    }

    #[test]
    fn test_from_into() {
        let raw_n = BoxedUint::from(101u64);
        let n_odd = Odd::new(raw_n.clone()).unwrap();
        let private_key = PrivateKey {
            pubkey_components: PublicKey {
                n: NonZero::new(raw_n.clone()).unwrap(),
                e: BoxedUint::from(200u64),
                n_params: BoxedMontyParams::new(n_odd),
            },
            d: BoxedUint::from(123u64),
            primes: vec![],
            precomputed: None,
        };
        let public_key: PublicKey = private_key.into();

        assert_eq!(PublicKeyParts::n(&public_key).as_ref(), &raw_n);
        assert_eq!(PublicKeyParts::e(&public_key), &BoxedUint::from(200u64));
        assert_eq!(PublicKeyParts::e_bytes(&public_key), [200].into());
        assert_eq!(PublicKeyParts::n_bytes(&public_key), [101].into());
    }

    fn test_key_basics(private_key: &PrivateKey) {
        private_key.validate().expect("invalid private key");

        assert_eq!(
            private_key.d().bits_precision(),
            private_key.n_bits_precision()
        );

        if private_key.primes().len() > 2 {
            for prime in private_key.primes() {
                assert_eq!(prime.bits_precision(), private_key.n_bits_precision());
            }
        }

        assert!(
            PrivateKeyParts::d(private_key) < PublicKeyParts::n(private_key).as_ref(),
            "private exponent too large"
        );

        let pub_key: PublicKey = private_key.clone().into();
        let m = BoxedUint::from(42u64);
        let c = rsa_encrypt(&pub_key, &m).expect("encryption successful");

        let m2 = rsa_decrypt_and_check::<ChaCha8Rng>(private_key, None, &c)
            .expect("unable to decrypt without blinding");
        assert_eq!(m, m2);
        let mut rng = ChaCha8Rng::from_seed([42; 32]);
        let m3 = rsa_decrypt_and_check(private_key, Some(&mut rng), &c)
            .expect("unable to decrypt with blinding");
        assert_eq!(m, m3);
    }

    #[test]
    fn private_component_widths() {
        // A 65-bit modulus and a 61-bit private exponent. Minimal storage for `d` is narrower than
        // storage for `n` on both 32-bit and 64-bit targets.
        let n = BoxedUint::from(18_446_744_116_659_224_501u128);
        let e = BoxedUint::from(65537u64);
        let d = BoxedUint::from(1_721_699_869_695_564_833u64);
        let p = BoxedUint::from(4_294_967_291u64);
        let q = BoxedUint::from(4_294_967_311u64);

        for input_bits in [64, 128, 256] {
            for reverse in [false, true] {
                let mut primes = vec![
                    (&p).resize_unchecked(input_bits),
                    (&q).resize_unchecked(input_bits),
                ];

                if reverse {
                    primes.reverse();
                }

                let mut key = PrivateKey::from_components(
                    (&n).resize_unchecked(256),
                    e.clone(),
                    (&d).resize_unchecked(input_bits),
                    primes,
                )
                .unwrap();
                assert_eq!(key.d(), &d);
                let prime_precision =
                    BoxedUint::zero_with_precision(key.n().bit_width().div_ceil(2))
                        .bits_precision();
                for prime in key.primes() {
                    assert_eq!(prime.bits_precision(), prime_precision);
                }
                assert_eq!(key.dp().expect("dp").bits_precision(), prime_precision);
                assert_eq!(key.dq().expect("dq").bits_precision(), prime_precision);
                assert_eq!(key.qinv().expect("qinv").bits_precision(), prime_precision);

                for clear_precomputed in [false, true] {
                    if clear_precomputed {
                        key.clear_precomputed();
                    }
                    test_key_basics(&key);

                    // This representative does not fit in `d`'s original 64-bit buffer.
                    // With odd `e` and `d`, both RSA operations fix `n-1`.
                    let c = key.n().as_ref().wrapping_sub(BoxedUint::one());
                    let m = rsa_decrypt_and_check::<ChaCha8Rng>(&key, None, &c).unwrap();
                    assert_eq!(m, c);
                }

                key.precompute().unwrap();
                test_key_basics(&key);
            }
        }

        // Resizing must reject significant high bits instead of silently truncating them into the
        // valid private exponent above.
        let oversized = d
            .resize_unchecked(256)
            .wrapping_add(&(BoxedUint::one_with_precision(256) << 128));

        assert_eq!(
            PrivateKey::from_components(n, e, oversized, vec![p, q]),
            Err(Error::InvalidExponent)
        );
    }

    macro_rules! key_generation {
        ($name:ident, $multi:expr, $size:expr) => {
            #[test]
            fn $name() {
                let mut rng = ChaCha8Rng::from_seed([42; 32]);
                let exp = BoxedUint::from(PrivateKey::EXP);

                for _ in 0..10 {
                    let components =
                        generate_multi_prime_key_with_exp(&mut rng, $multi, $size, exp.clone())
                            .unwrap();
                    let private_key = PrivateKey::from_components(
                        components.n.get(),
                        components.e,
                        components.d,
                        components.primes,
                    )
                    .unwrap();
                    assert_eq!(PublicKeyParts::n(&private_key).bit_width(), $size);

                    test_key_basics(&private_key);
                }
            }
        };
    }

    key_generation!(key_generation_128, 2, 128);
    key_generation!(key_generation_1024, 2, 1024);

    key_generation!(key_generation_multi_3_256, 3, 256);

    key_generation!(key_generation_multi_4_64, 4, 64);

    key_generation!(key_generation_multi_5_64, 5, 64);
    key_generation!(key_generation_multi_8_576, 8, 576);
    key_generation!(key_generation_multi_16_1024, 16, 1024);

    #[test]
    fn test_negative_decryption_value() {
        let bits = 128;
        let private_key = PrivateKey::from_components(
            BoxedUint::from_le_slice(
                &[
                    99, 192, 208, 179, 0, 220, 7, 29, 49, 151, 75, 107, 75, 73, 200, 180,
                ],
                bits,
            )
            .unwrap(),
            BoxedUint::from_le_slice(&[1, 0, 1, 0, 0, 0, 0, 0], 64).unwrap(),
            BoxedUint::from_le_slice(
                &[
                    81, 163, 254, 144, 171, 159, 144, 42, 244, 133, 51, 249, 28, 12, 63, 65,
                ],
                bits,
            )
            .unwrap(),
            vec![
                BoxedUint::from_le_slice(&[105, 101, 60, 173, 19, 153, 3, 192], bits / 2).unwrap(),
                BoxedUint::from_le_slice(&[235, 65, 160, 134, 32, 136, 6, 241], bits / 2).unwrap(),
            ],
        )
        .unwrap();

        for _ in 0..1000 {
            test_key_basics(&private_key);
        }
    }

    #[test]
    #[cfg(all(feature = "hazmat", feature = "serde"))]
    fn test_serde() {
        use rand::rngs::ChaCha8Rng;
        use rand_core::SeedableRng;
        use serde_test::{Configure, Token, assert_ser_tokens, assert_tokens};

        let mut rng = ChaCha8Rng::from_seed([42; 32]);
        let priv_key = PrivateKey::new_unchecked(&mut rng, 64).expect("failed to generate key");

        let priv_tokens = [Token::Str(concat!(
            "3056020100300d06092a864886f70d010101050004423040020100020900a",
            "b240c3361d02e370203010001020811e54a15259d22f9020500ceff5cf302",
            "0500d3a7aaad020500ccaddf17020500cb529d3d020500bb526d6f"
        ))];
        assert_tokens(&priv_key.clone().readable(), &priv_tokens);

        let pub_tokens = [Token::Str(
            "3024300d06092a864886f70d01010105000313003010020900ab240c3361d02e370203010001",
        )];
        // Small public keys can still be serialized, but default decoding rejects them.
        assert_ser_tokens(&PublicKey::from(priv_key.clone()).readable(), &pub_tokens);
    }

    #[test]
    fn invalid_coeff_private_key_regression() {
        use base64ct::{Base64, Encoding};

        let n = Base64::decode_vec(
            "wC8GyQvTCZOK+iiBR5fGQCmzRCTWX9TQ3aRG5gGFk0wB6EFoLMAyEEqeG3gS8xhA\
             m2rSWYx9kKufvNat3iWlbSRVqkcbpVAYlj2vTrpqDpJl+6u+zxFYoUEBevlJJkAh\
             l8EuCccOA30fVpcfRvXPTtvRd3yFT9E9EwZljtgSI02w7gZwg7VIxaGeajh5Euz6\
             ZVQZ+qNRKgXrRC7gPRqVyI6Dt0Jc+Su5KBGNn0QcPDzOahWha1ieaeMkFisZ9mdp\
             sJoZ4tw5eicLaUomKzALHXQVt+/rcZSrCd6/7uUo11B/CYBM4UfSpwXaL88J9AE6\
             A5++no9hmJzaF2LLp+Qwx4yY3j9TDutxSAjsraxxJOGZ3XyA9nG++Ybt3cxZ5fP7\
             ROjxCfROBmVv5dYn0O9OBIqYeCH6QraNpZMadlLNIhyMv8Y+P3r5l/PaK4VJaEi5\
             pPosnEPawp0W0yZDzmjk2z1LthaRx0aZVrAjlH0Rb/6goLUQ9qu1xsDtQVVpN4A8\
             9ZUmtTWORnnJr0+595eHHxssd2gpzqf4bPjNITdAEuOCCtpvyi4ls23zwuzryUYj\
             cUOEnsXNQ+DrZpLKxdtsD/qNV/j1hfeyBoPllC3cV+6bcGOFcVGbjYqb+Kw1b0+j\
             L69RSKQqgmS+qYqr8c48nDRxyq3QXhR8qtzUwBFSLVk=",
        )
        .unwrap();
        let e = Base64::decode_vec("AQAB").unwrap();
        let d = Base64::decode_vec(
            "qQazSQ+FRN7nVK1bRsROMRB8AmsDwLVEHivlz1V3Td2Dr+oW3YUMgxedhztML1Id\
             QJPq/ad6qErJ6yRFNySVIjDaxzBTOEoB1eHa1btOnBJWb8rVvvjaorixvJ6Tn3i4\
             EuhsvVy9DoR1k4rGj3qSIiFjUVvLRDAbLyhpGgEfsr0Z577yJmTC5E8JLRMOKX8T\
             mxsk3jPVpsgd65Hu1s8S/ZmabwuHCf9SkdMeY/1bd/9i7BqqJeeDLE4B5x1xcC3z\
             3scqDUTzqGO+vZPhjgprPDRlBamVwgenhr7KwCn8iaLamFinRVwOAag8BeBqOJj7\
             lURiOsKQa9FIX1kdFUS1QMQxgtPycLjkbvCJjriqT7zWKsmJ7l8YLs6Wmm9/+QJR\
             wNCEVdMTXKfCP1cJjudaiskEQThfUldtgu8gUDNYbQ/Filb2eKfiX4h1TiMxZqUZ\
             HVZyb9nShbQoXJ3vj/MGVF0QM8TxhXM8r2Lv9gDYU5t9nQlUMLhs0jVjai48jHAB\
             bFNyH3sEcOmJOIwJrCXw1dzG7AotwyaEVUHOmL04TffmwCFfnyrLjbFgnyOeoyII\
             BYjcY7QFRm/9nupXMTH5hZ2qrHfCJIp0KK4tNBdQqmnHapFl5l6Le1s4qBS5bEIz\
             jitobLvAFm9abPlDGfxmY6mlrMK4+nytwF9Ct7wc1AE=",
        )
        .unwrap();
        let primes = [
            Base64::decode_vec(
                "9kQWEAzsbzOcdPa+s5wFfw4XDd7bB1q9foZ31b1+TNjGNxbSBCFlDF1q98vwpV6n\
                 M8bWDh/wtbNoETSQDgpEnYOQ26LWEw6YY1+q1Q2GGEFceYUf+Myk8/vTc8TN6Zw0\
                 bKZBWy10Qo8h7xk4JpzuI7NcxvjJYTkS9aErFxi3vVH0aiZC0tmfaCqr8a2rJxyV\
                 wqreRpOjwAWrotMsf2wGsF4ofx5ScoFy5GB5fJkkdOrW1LyTvZAUCX3cstPr19+T\
                 NC5zZOk7WzZatnCkN5H5WzalWtZuu0oVL205KPOa3R8V2yv5e6fm0v5fTmqSuvjm\
                 aMJLXCN4QJkmIzojO99ckQ==",
            )
            .unwrap(),
            Base64::decode_vec(
                "x8exdMjVA2CiI+Thx7loHtVcevoeE2sZ7btRVAvmBqo+lkHwxb7FHRnWvuj6eJSl\
                 D2f0T50EewIhhiW3R9BmktCk7hXjbSCnC1u9Oxc1IAUm/7azRqyfCMx43XhLxpD+\
                 xkBCpWkKDLxGczsRwTuaP3lKS3bSdBrNlGmdblubvVBIq4YZ2vXVlnYtza0cS+dg\
                 CK7BGTqUsrCUd/ZbIvwcwZkZtpkhj1KQfto9X/0OMurBzAqbkeq1cyRHXHkOfN/q\
                 bUIIRqr9Ii7Eswf9Vk8xp2O1Nt8nzcYS9PFD12M5eyaeFEkEYfpNMNGuTzp/31oq\
                 VjbpoCxS6vuWAZyADxhISQ==",
            )
            .unwrap(),
            Base64::decode_vec(
                "is7d0LY4HoXszlC2NO7gejkq7XqL4p1W6hZJPYTNx+r37t1CC2n3Vvzg6kNdpRix\
                 DhIpXVTLjN9O7UO/XuqSumYKJIKoP52eb4Tg+a3hw5Iz2Zsb5lUTNSLgkQSBPAf7\
                 1LHxbL82JL4g1nBUog8ae60BwnVArThKY4EwlJguGNw09BAU4lwf6csDl/nX2vfV\
                 wiAloYpeZkHL+L8m+bueGZM5KE2jEz+7ztZCI+T+E5i69rZEYDjx0lfLKlEhQlCW\
                 3HbCPELqXgNJJkRfi6MP9kXa9lSfnZmoT081RMvqonB/FUa4HOcKyCrw9XZEtnbN\
                 CIdbitfDVEX+pSSD7596wQ==",
            )
            .unwrap(),
            Base64::decode_vec(
                "GPs0injugfycacaeIP5jMa/WX55VEnKLDHom4k6WlfDF4L4gIGoJdekcPEUfxOI5\
                 faKvHyFwRP1wObkPoRBDM0qZxRfBl4zEtpvjHrd5MibSyJkM8+J0BIKk/nSjbRIG\
                 eb3hV5O56PvGB3S0dKhCUnuVObiC+ne7izplsD4OTG70l1Yud33UFntyoMxrxGYL\
                 USqhBMmZfHquJg4NOWOzKNY/K+EcHDLj1Kjvkcgv9Vf7ocsVxvpFdD9uGPceQ6kw\
                 RDdEl6mb+6FDgWuXVyqR9+904oanEIkbJ7vfkthagLbEf57dyG6nJlqh5FBZWxGI\
                 R72YGypPuAh7qnnqXXjY2Q==",
            )
            .unwrap(),
            Base64::decode_vec(
                "CUWC+hRWOT421kwRllgVjy6FYv6jQUcgDNHeAiYZnf5HjS9iK2ki7v8G5dL/0f+Y\
                 f+NhE/4q8w4m8go51hACrVpP1p8GJDjiT09+RsOzITsHwl+ceEKoe56ZW6iDHBLl\
                 rNw5/MtcYhKpjNU9KJ2udm5J/c9iislcjgckrZG2IB8ADgXHMEByZ5DgaMl4AKZ1\
                 Gx8/q6KftTvmOT5rNTMLi76VN5KWQcDWK/DqXiOiZHM7Nr4dX4me3XeRgABJyNR8\
                 Fqxj3N1+HrYLe/zs7LOaK0++F9Ul3tLelhrhsvLxei3oCZkF9A/foD3on3luYA+1\
                 cRcxWpSY3h2J4/22+yo4+Q==",
            )
            .unwrap(),
        ];

        let e = BoxedUint::from_be_slice(&e, 64).unwrap();

        let bits = 4096;
        let n = BoxedUint::from_be_slice(&n, bits).unwrap();
        let d = BoxedUint::from_be_slice(&d, bits).unwrap();
        let primes = primes
            .iter()
            .map(|p| BoxedUint::from_be_slice(p, bits / 2).unwrap())
            .collect();
        let res = PrivateKey::from_components(n, e, d, primes);
        assert_eq!(res, Err(Error::InvalidModulus));
    }

    #[test]
    fn reject_oversized_private_key() {
        // -----BEGIN PUBLIC KEY-----
        // MIIEKjANBgkqhkiG9w0BAQEFAAOCBBcAMIIEEgKCBAkAqQn6O7pd9ioQJEOwS2sh
        // nD2bM3+PaLovro+OKOE9t7jxrp+b9Xq81oeT6zN5u5yPewa+V08ZsAJQEbF9D5AM
        // UZkHZc/sW/XAItC8CojQhHoCQfjOXZpONmGsQxnSJNgwLV5TDVKUApbQIPzIm9yD
        // wOvl1yXIypaRINHzthz36ysHmaHlNVZZQ40BHVkOiUd+ws7W9U9vHN0QcaSHC8lH
        // UEqb/Iyb0FSmZs+qbm4NXyaI90oloAFftOnt8VFbHfT/TXS0VwMyescxFsuvcuTr
        // Xx8EYc9TuJThW22wBAFOK6SpftgtZ6i4WJqk0F8JrTwZ3TyhzKsPRwe8KeNmtmqY
        // oaGiPj9lUOc928QzOyTETVd8pV7UpnaOe9Q4WHL0QmnXn61pCu4qpoLuK8jB+IO7
        // xifRZHj3PMfsjJurZ4MF57LgpSrI60ekYNh1o6ViXODHQuzGxzTaF3n/7GITDBQX
        // DRTlGuQH77hykxFqPclRGI0wxECPKasxpzjhiaTua9eipKedXB+o5XFyosnDt/X4
        // Ygqxj/q2/18LPuQgFLqWRzsHd4TdVQyiq9xCmzIoGUjAPz1Q8cjIXRpUnp2rZQjE
        // SCLeTjewrGNbjSMDUhdF5M2OBRmn7Q8XHHCUxT9fY/BZeydeE54KvEdEkomxkbXo
        // hHKEmbWeEdhp78h04/xW364p1Nu9Y49w7gtO+9nmwKcpNJq32M6Qb0d2dQ3wJ0oI
        // I9ml+n/DTna+IIwwbI8UOFEI4KZQzZaqmNv3TzGmpnocHK7eMyEtAUeQZUIGrPmr
        // FQEmKZn9rkgr/2Hw8T20q7e0lE65Is69vTP2wXm17B5zKFYska41jJoZ6jIpbMOt
        // uVPZV3SoGYM39Z4Ax3JaGZE0L/dQ6lJJhdFUACFIQXwNWqzd7srnvbym4hLqoPuM
        // hjkUtTcv6YODEk7LB2FLDcymmH/zCL3w4VSi4+HyZZ13gM7Cz8WmkX4H+jeL0+Ja
        // QyG1CzqV/HA78vUpJv/bb/J1+X1i/1HltLeTjteY4rBhVT1cxBoVBGQaCwindAs+
        // Fjcp+8cAK+/3pMQghnkrGHzrx9YIYoOGXs4vQIMGngYaTa7aXAaft4fWjg4EeSjd
        // rZwqqrPNuUcEuneFP9RPffj8f3vkhqCFgoVBfVM7ontu2d2nRu/hhAkgT13Uc68J
        // dM2imBvHAo6DDUt6msWCAMOAEXYuO7aA+n3eettnBqtECoQAoCJdCHCebjIploMB
        // XMLXyseGtLK9arI48hDvcxSlf7/1lkBB6LgNQmQJ7925TDipiYQIZ63f4d5Z2JCp
        // W0vUkwzrH4iPb2hy+TBQSOw1kvjLyG/lHWjzDQa60xxVW9u59DxQueHsNEMHUORD
        // 1oFXvFLe/AllAgMBAAE=
        // -----END PUBLIC KEY-----

        let n = BoxedUint::from_be_slice(
            &hex!(
                "a909fa3bba5df62a102443b04b6b219c3d9b337f8f68ba2fae8f8e28e13db7b8
                 f1ae9f9bf57abcd68793eb3379bb9c8f7b06be574f19b0025011b17d0f900c51
                 990765cfec5bf5c022d0bc0a88d0847a0241f8ce5d9a4e3661ac4319d224d830
                 2d5e530d52940296d020fcc89bdc83c0ebe5d725c8ca969120d1f3b61cf7eb2b
                 0799a1e5355659438d011d590e89477ec2ced6f54f6f1cdd1071a4870bc94750
                 4a9bfc8c9bd054a666cfaa6e6e0d5f2688f74a25a0015fb4e9edf1515b1df4ff
                 4d74b45703327ac73116cbaf72e4eb5f1f0461cf53b894e15b6db004014e2ba4
                 a97ed82d67a8b8589aa4d05f09ad3c19dd3ca1ccab0f4707bc29e366b66a98a1
                 a1a23e3f6550e73ddbc4333b24c44d577ca55ed4a6768e7bd4385872f44269d7
                 9fad690aee2aa682ee2bc8c1f883bbc627d16478f73cc7ec8c9bab678305e7b2
                 e0a52ac8eb47a460d875a3a5625ce0c742ecc6c734da1779ffec62130c14170d
                 14e51ae407efb87293116a3dc951188d30c4408f29ab31a738e189a4ee6bd7a2
                 a4a79d5c1fa8e57172a2c9c3b7f5f8620ab18ffab6ff5f0b3ee42014ba96473b
                 077784dd550ca2abdc429b32281948c03f3d50f1c8c85d1a549e9dab6508c448
                 22de4e37b0ac635b8d2303521745e4cd8e0519a7ed0f171c7094c53f5f63f059
                 7b275e139e0abc47449289b191b5e884728499b59e11d869efc874e3fc56dfae
                 29d4dbbd638f70ee0b4efbd9e6c0a729349ab7d8ce906f4776750df0274a0823
                 d9a5fa7fc34e76be208c306c8f14385108e0a650cd96aa98dbf74f31a6a67a1c
                 1caede33212d014790654206acf9ab1501262999fdae482bff61f0f13db4abb7
                 b4944eb922cebdbd33f6c179b5ec1e7328562c91ae358c9a19ea32296cc3adb9
                 53d95774a8198337f59e00c7725a1991342ff750ea524985d154002148417c0d
                 5aacddeecae7bdbca6e212eaa0fb8c863914b5372fe98383124ecb07614b0dcc
                 a6987ff308bdf0e154a2e3e1f2659d7780cec2cfc5a6917e07fa378bd3e25a43
                 21b50b3a95fc703bf2f52926ffdb6ff275f97d62ff51e5b4b7938ed798e2b061
                 553d5cc41a1504641a0b08a7740b3e163729fbc7002beff7a4c42086792b187c
                 ebc7d6086283865ece2f4083069e061a4daeda5c069fb787d68e0e047928ddad
                 9c2aaab3cdb94704ba77853fd44f7df8fc7f7be486a0858285417d533ba27b6e
                 d9dda746efe18409204f5dd473af0974cda2981bc7028e830d4b7a9ac58200c3
                 8011762e3bb680fa7dde7adb6706ab440a8400a0225d08709e6e32299683015c
                 c2d7cac786b4b2bd6ab238f210ef7314a57fbff5964041e8b80d426409efddb9
                 4c38a989840867addfe1de59d890a95b4bd4930ceb1f888f6f6872f9305048ec
                 3592f8cbc86fe51d68f30d06bad31c555bdbb9f43c50b9e1ec34430750e443d6
                 8157bc52defc0965"
            ),
            8256,
        )
        .unwrap();

        let e = BoxedUint::from(65_537u64);

        assert_eq!(PublicKey::new(n, e).err().unwrap(), Error::ModulusTooLarge);
    }

    #[test]
    #[cfg(feature = "encoding")]
    fn build_key_from_primes() {
        const RSA_2048_PRIV_DER: &[u8] = &[0x30u8,0x82,0x04,0xbd,0x02,0x01,0x00,0x30,0x0d,0x06,0x09,0x2a,0x86,0x48,0x86,0xf7,0x0d,0x01,0x01,0x01,0x05,0x00,0x04,0x82,0x04,0xa7,0x30,0x82,0x04,0xa3,0x02,0x01,0x00,0x02,0x82,0x01,0x01,0x00,0xb6,0xc4,0x2c,0x51,0x5f,0x10,0xa6,0xaa,0xf2,0x82,0xc6,0x3e,0xdb,0xe2,0x42,0x43,0xa1,0x70,0xf3,0xfa,0x26,0x33,0xbd,0x48,0x33,0x63,0x7f,0x47,0xca,0x4f,0x6f,0x36,0xe0,0x3a,0x5d,0x29,0xef,0xc3,0x19,0x1a,0xc8,0x0f,0x39,0x0d,0x87,0x4b,0x39,0xe3,0x0f,0x41,0x4f,0xce,0xc1,0xfc,0xa0,0xed,0x81,0xe5,0x47,0xed,0xc2,0xcd,0x38,0x2c,0x76,0xf6,0x1c,0x90,0x18,0x97,0x3d,0xb9,0xfa,0x53,0x79,0x72,0xa7,0xc7,0x01,0xf6,0xb7,0x7e,0x09,0x82,0xdf,0xc1,0x5f,0xc0,0x19,0x27,0xee,0x5e,0x7c,0xd9,0x4b,0x4f,0x59,0x9f,0xf0,0x70,0x13,0xa7,0xc8,0x28,0x1b,0xdf,0x22,0xdc,0xbc,0x9a,0xd7,0xca,0xbb,0x7c,0x43,0x11,0xc9,0x82,0xf5,0x8e,0xdb,0x72,0x13,0xad,0x45,0x58,0xb3,0x32,0x26,0x6d,0x74,0x3a,0xed,0x81,0x92,0xd1,0x88,0x4c,0xad,0xb8,0xb1,0x47,0x39,0xa8,0xda,0xda,0x66,0xdc,0x97,0x08,0x06,0xd9,0xc7,0xac,0x45,0x0c,0xb1,0x3d,0x0d,0x7c,0x57,0x5f,0xb1,0x98,0x53,0x4f,0xc6,0x1b,0xc4,0x1b,0xc0,0xf0,0x57,0x4e,0x0e,0x01,0x30,0xc7,0xbb,0xbf,0xbd,0xfd,0xc9,0xf6,0xa6,0xe2,0xe3,0xe2,0xaf,0xf1,0xcb,0xea,0xc8,0x9b,0xa5,0x78,0x84,0x52,0x8d,0x55,0xcf,0xb0,0x83,0x27,0xa1,0xe8,0xc8,0x9f,0x4e,0x00,0x3c,0xf2,0x88,0x8e,0x93,0x32,0x41,0xd9,0xd6,0x95,0xbc,0xbb,0xac,0xdc,0x90,0xb4,0x4e,0x3e,0x09,0x5f,0xa3,0x70,0x58,0xea,0x25,0xb1,0x3f,0x5e,0x29,0x5c,0xbe,0xac,0x6d,0xe8,0x38,0xab,0x8c,0x50,0xaf,0x61,0xe2,0x98,0x97,0x5b,0x87,0x2f,0x02,0x03,0x01,0x00,0x01,0x02,0x82,0x01,0x00,0x7e,0xcc,0x83,0x62,0xc0,0xed,0xb0,0x74,0x11,0x64,0x21,0x5e,0x22,0xf7,0x4a,0xb9,0xd9,0x1b,0xa0,0x69,0x00,0x70,0x0c,0xf6,0x36,0x90,0xe5,0x11,0x4d,0x8e,0xe6,0xbd,0xcf,0xbb,0x2e,0x3f,0x96,0x14,0x69,0x2a,0x67,0x7a,0x08,0x3f,0x16,0x8a,0x5e,0x52,0xe5,0x96,0x8e,0x64,0x07,0xb9,0xd9,0x7c,0x6e,0x0e,0x40,0x64,0xf8,0x2d,0xa0,0xb7,0x58,0xa1,0x4f,0x17,0xb9,0xb7,0xd4,0x1f,0x5f,0x48,0xe2,0x8d,0x65,0x51,0x70,0x4f,0x56,0xe6,0x9e,0x7a,0xa9,0xfa,0x63,0x0f,0xc7,0x64,0x28,0xc0,0x6d,0x25,0xe4,0x55,0xdc,0xfc,0x55,0xb7,0xac,0x2b,0x4f,0x76,0x64,0x3f,0xde,0xd3,0xfe,0x15,0xff,0x78,0xab,0xb2,0x7e,0x65,0xac,0xc4,0xaa,0xd0,0xbd,0xf6,0xdb,0x27,0xef,0x60,0xa6,0x91,0x0c,0x5c,0x4a,0x08,0x5e,0xd4,0x32,0x75,0xab,0x19,0xc1,0xd9,0x97,0xa3,0x2c,0x6e,0xff,0xce,0x7d,0xf2,0xd1,0x93,0x5f,0x6e,0x60,0x1e,0xed,0xe1,0x61,0xa1,0x2b,0x5c,0xc2,0x7c,0xa2,0x1f,0x81,0xd2,0xc9,0x9c,0x3d,0x1e,0xa0,0x8e,0x90,0xe3,0x05,0x3a,0xb0,0x9b,0xef,0xa7,0x24,0xde,0xf0,0xd0,0xc3,0xa3,0xc1,0xe9,0x74,0x0c,0x0d,0x9f,0x76,0x12,0x6a,0x14,0x9e,0xc0,0xaa,0x7d,0x80,0x78,0x20,0x54,0x84,0x25,0x4d,0x95,0x1d,0xb0,0x7c,0x4c,0xf9,0x1f,0xb6,0x45,0x4c,0x09,0x65,0x88,0xfd,0x59,0x24,0xdb,0xab,0xeb,0x35,0x9c,0xa2,0x02,0x52,0x68,0xd0,0x04,0xf9,0xd6,0x6e,0xb3,0xd6,0xf7,0xad,0xc1,0x13,0x9b,0xad,0x40,0xf1,0x6d,0xde,0x63,0x9e,0x11,0x64,0x73,0x76,0xc1,0x02,0x81,0x81,0x00,0xdc,0xc0,0x61,0x24,0x2d,0x4e,0x92,0xaf,0xae,0xe7,0x2a,0xc5,0x13,0xca,0x65,0xb9,0xf7,0x70,0x36,0xf9,0xbd,0x7e,0x0e,0x6e,0x61,0x46,0x1a,0x7e,0xf7,0x65,0x42,0x25,0xec,0x15,0x3c,0x7e,0x5c,0x31,0xa6,0x15,0x7a,0x6e,0x5a,0x13,0xff,0x6e,0x17,0x8e,0x87,0x58,0xc1,0xcb,0x33,0xd9,0xd6,0xbb,0xe3,0x17,0x9e,0xf1,0x89,0x98,0xe4,0x22,0xec,0xdc,0xbe,0xd7,0x8f,0x4e,0xcf,0xdb,0xe5,0xf4,0xfc,0xd8,0xae,0xc2,0xc9,0xd0,0xdc,0x86,0x47,0x3c,0xa9,0xbd,0x16,0xd9,0xd2,0x38,0xd2,0x1f,0xb5,0xdd,0xef,0xbe,0xb1,0x43,0xca,0x61,0xd0,0xbd,0x6a,0xa8,0xd9,0x1f,0x33,0xa0,0x97,0x79,0x0e,0x96,0x40,0xdb,0xc9,0x10,0x85,0xdc,0x5f,0x26,0x34,0x3b,0xa3,0x13,0x8f,0x6b,0x2d,0x67,0x02,0x81,0x81,0x00,0xd3,0xf3,0x14,0x75,0x7e,0x40,0xe9,0x54,0x83,0x6f,0x92,0xbe,0x24,0x23,0x6a,0xf2,0xf0,0xda,0x04,0xa3,0x46,0x53,0xc1,0x80,0xaf,0x67,0xe9,0x60,0x08,0x6d,0x93,0xfd,0xe6,0x5c,0xb2,0x3e,0xfd,0x9d,0x09,0x37,0x47,0x62,0xf5,0x98,0x1e,0x36,0x18,0x49,0xaf,0x68,0xcd,0xd7,0x53,0x94,0xff,0x6a,0x4e,0x06,0xeb,0x69,0xb2,0x09,0xe4,0x22,0x8d,0xb2,0xdf,0xa7,0x0e,0x40,0xf7,0xf9,0x75,0x0a,0x52,0x81,0x76,0x64,0x7b,0x78,0x8d,0x0e,0x57,0x77,0xa2,0xcb,0x8b,0x22,0xe3,0xcd,0x26,0x7f,0xf7,0x0b,0x4f,0x3b,0x02,0xd3,0xaa,0xfb,0x0e,0x18,0xc5,0x90,0xa5,0x64,0xb0,0x31,0x88,0xb0,0xaa,0x5f,0xc4,0x81,0x56,0xb0,0x76,0x22,0x21,0x42,0x43,0xbd,0x12,0x27,0xef,0xa7,0xf2,0xf9,0x02,0x81,0x81,0x00,0xce,0x68,0xb7,0xac,0x1b,0x0d,0x10,0x0d,0x63,0x6e,0x55,0x48,0x87,0x53,0xc5,0xc0,0x98,0x43,0xfd,0xb3,0x90,0xe2,0x70,0x5d,0xf7,0x68,0x94,0x57,0xc9,0xbd,0x8d,0x97,0x65,0xe3,0x09,0x78,0x61,0x7e,0x2e,0xfc,0x80,0x48,0xf4,0xc3,0x24,0x20,0x6d,0xb8,0x60,0x87,0xb6,0x54,0xe9,0x7b,0xb3,0xd4,0x64,0xe7,0xee,0x3f,0x8c,0xd8,0x3f,0xe1,0x04,0x36,0xf7,0xdf,0x18,0xe9,0xa9,0x63,0xc4,0xe6,0x49,0x11,0xd6,0x7e,0xde,0x34,0x04,0x2f,0x2e,0x26,0xe3,0xd3,0xa1,0xad,0x34,0x6a,0xda,0xd6,0xb9,0xb7,0xf6,0x77,0x08,0xcb,0x09,0x4e,0x62,0xde,0xe9,0xff,0x4d,0x5d,0x66,0x69,0xaf,0x98,0x8a,0xf2,0x25,0x5d,0x1c,0xe8,0xed,0x31,0x7c,0x6a,0x7d,0x86,0x91,0xda,0x35,0x4d,0x12,0xdb,0x02,0x81,0x80,0x25,0xf6,0xe5,0x94,0x42,0x20,0x28,0x6b,0x4d,0xfb,0xbf,0x42,0x35,0xc0,0xee,0x58,0x43,0xd2,0x19,0x80,0x91,0x89,0x51,0x20,0xd6,0xca,0x7b,0x20,0x0b,0x82,0x6d,0x3e,0xce,0x73,0x8e,0x2e,0x00,0x49,0x8f,0xac,0x0a,0x2a,0x6c,0xa9,0x69,0xc7,0xf0,0xc3,0xca,0x1a,0xb0,0xbc,0x40,0x29,0x71,0x32,0xbe,0x75,0x38,0xd7,0xbe,0xdf,0x4c,0xb0,0xef,0xc6,0xb9,0x8e,0xf7,0xdb,0xa5,0x4f,0x56,0xaa,0x99,0xaa,0xbc,0xe5,0x34,0xc4,0x9c,0x27,0x94,0x7d,0x46,0x78,0xc5,0x1c,0x63,0xc7,0x8c,0x7c,0xe1,0x68,0x72,0x31,0xb4,0xc8,0xeb,0x58,0x7a,0xe6,0xef,0x04,0x80,0xcb,0xaf,0x4f,0xc0,0x17,0x3c,0xfd,0x58,0x7a,0x7e,0x67,0xaf,0x51,0x5f,0xb9,0xb9,0xde,0x75,0x11,0x18,0x39,0x72,0x29,0x02,0x81,0x80,0x31,0x99,0x54,0x06,0xd4,0x06,0x20,0x7c,0xad,0xea,0xea,0x35,0xb3,0x8d,0x04,0x0c,0x5f,0x8a,0x9a,0x1a,0xe0,0x82,0x7e,0x9e,0xd0,0x6b,0x15,0x3d,0x83,0xb6,0x82,0x19,0x35,0xb4,0xb3,0x6a,0x82,0xbe,0x9d,0x56,0xc7,0x91,0xb5,0x8c,0x27,0x27,0x1a,0x57,0x93,0xd5,0x3a,0x1d,0x65,0x7c,0x08,0x99,0x79,0x60,0xb1,0x43,0x3e,0x51,0x71,0x98,0x7f,0x45,0x2f,0x14,0x4a,0x7c,0x72,0x30,0x6d,0x63,0xe1,0xd3,0xff,0xc0,0xb7,0x1b,0x75,0xab,0x08,0xf2,0xe4,0x5a,0x48,0x2e,0x98,0x84,0x51,0xcb,0xe4,0x78,0xe1,0x2e,0xb2,0x28,0xd0,0x74,0x56,0xc9,0x24,0xb6,0x6f,0x6c,0xed,0x04,0x8d,0x85,0x3f,0x53,0x3e,0x31,0xa6,0x86,0x14,0xf1,0xc3,0xce,0x6d,0x8e,0xc9,0x98,0x3c,0xe7,0x2a,0xf7];
        let ref_key = PrivateKey::from_pkcs8_der(RSA_2048_PRIV_DER).unwrap();
        assert_eq!(ref_key.validate(), Ok(()));

        let primes: Vec<_> = PrivateKeyParts::primes(&ref_key)
            .iter()
            .map(|p| p.as_ref().clone())
            .collect();

        let exp = PublicKeyParts::e(&ref_key);
        let key =
            PrivateKey::from_primes(primes, exp.clone()).expect("failed to import key from primes");
        assert_eq!(key.validate(), Ok(()));

        assert_eq!(PublicKeyParts::n(&key), PublicKeyParts::n(&ref_key));

        assert_eq!(PrivateKeyParts::dp(&key), PrivateKeyParts::dp(&ref_key));
        assert_eq!(PrivateKeyParts::dq(&key), PrivateKeyParts::dq(&ref_key));

        assert_eq!(PrivateKeyParts::d(&key), PrivateKeyParts::d(&ref_key));
    }

    #[test]
    #[cfg(feature = "encoding")]
    fn build_key_from_p_q() {
        const RSA_2048_SP800_PRIV_DER: &[u8] =
            &[0x30u8,0x82,0x04,0xbd,0x02,0x01,0x00,0x30,0x0d,0x06,0x09,0x2a,0x86,0x48,0x86,0xf7,0x0d,0x01,0x01,0x01,0x05,0x00,0x04,0x82,0x04,0xa7,0x30,0x82,0x04,0xa3,0x02,0x01,0x00,0x02,0x82,0x01,0x01,0x00,0xa6,0x13,0x2a,0xa6,0xbf,0xe0,0xa0,0x59,0xab,0x4b,0xf3,0xad,0x9b,0x31,0xf8,0x69,0xf9,0x6e,0xd7,0xe6,0x53,0x11,0x93,0x11,0xf5,0x8a,0x1a,0x60,0xc0,0xac,0x11,0xaa,0xbc,0x30,0xa9,0x65,0xfc,0x5a,0xe3,0x34,0x61,0x2c,0xe1,0x3b,0x1f,0x59,0x03,0xfa,0xe8,0xd4,0xa4,0x9a,0x4d,0x3a,0x0f,0x86,0xd9,0x11,0x6a,0xa0,0xa6,0x32,0x39,0x07,0xcb,0x92,0x95,0xcf,0x6b,0xe3,0xb1,0xa0,0xd5,0xfe,0x70,0x01,0x7d,0x32,0x9c,0x08,0x8a,0x4a,0x74,0x18,0x93,0xaf,0xc5,0x58,0xa4,0x7f,0xde,0x7b,0x90,0x27,0x7a,0x14,0x98,0x7b,0xad,0xcd,0xba,0x24,0x6b,0xfc,0x96,0xae,0x48,0x11,0x69,0xd5,0xe6,0x4e,0xc7,0x3a,0xe8,0x25,0xec,0xa3,0x8b,0xd3,0xcc,0xbb,0x6e,0x53,0x3a,0x4c,0x46,0xc5,0x3a,0x82,0x60,0x12,0x2a,0x4e,0x0e,0x0e,0x2f,0xb6,0x37,0x71,0x41,0xab,0x66,0x88,0x9a,0x00,0x05,0x13,0xe6,0x02,0xb7,0xd2,0x32,0xa5,0x92,0x49,0x0e,0x35,0x76,0x47,0x13,0xf4,0x1e,0x92,0xf9,0x1b,0x37,0x77,0x09,0x0f,0x0e,0x95,0x85,0xc2,0xa1,0x4b,0x18,0xbc,0x7a,0xd9,0x1d,0xa5,0x05,0x07,0xcd,0x9a,0x26,0x80,0xbe,0xd8,0xd4,0xe0,0x34,0x2a,0x03,0x0a,0x1a,0x32,0x6d,0x72,0x50,0x9d,0x3d,0x2f,0xb9,0x15,0x09,0x1f,0xee,0x79,0xaa,0xc9,0xa3,0xb2,0xa2,0xdf,0x63,0xd3,0x97,0x63,0x4c,0xa5,0x42,0x35,0xe5,0xb9,0x2f,0x17,0x65,0x97,0xa6,0x98,0xc2,0x68,0x32,0xa8,0x99,0x17,0x68,0x3f,0x23,0x9e,0x73,0xee,0x5d,0xca,0xa4,0x33,0x2f,0xf6,0xf7,0xef,0xc1,0x25,0x87,0xd1,0x02,0x03,0x01,0x00,0x01,0x02,0x81,0xff,0x1a,0x07,0xa7,0x07,0xbb,0xcb,0x59,0x56,0xb4,0xa2,0x92,0xef,0x03,0x04,0x32,0x0b,0x6a,0x2d,0x15,0x69,0xe4,0x5b,0x3c,0xd1,0xf3,0xca,0x51,0x98,0x18,0x9a,0xdf,0xaa,0x03,0x15,0x1d,0x77,0xfe,0xb5,0xc0,0x26,0xd5,0x94,0x53,0x39,0x11,0x10,0xc2,0xae,0xf1,0x0f,0x63,0x3d,0x4c,0x1d,0x6d,0x91,0x95,0x34,0x45,0xa2,0x28,0x6a,0x76,0xc5,0xe2,0x02,0x77,0xb8,0xab,0x10,0x65,0x26,0xf0,0x63,0x90,0xea,0xaa,0xd4,0xe3,0xdf,0xf2,0xcc,0xf8,0xa5,0x61,0x80,0x8b,0x4d,0xf9,0x7a,0x91,0x44,0x8c,0xb3,0xa3,0x4e,0xd7,0x17,0x8b,0x86,0x53,0x46,0xa2,0x26,0x54,0xf7,0xbc,0x13,0xfe,0xa2,0xa8,0x16,0x70,0xe3,0xaa,0xbf,0x46,0xf7,0xdb,0x52,0xb7,0x24,0x29,0x86,0xa1,0xfc,0x92,0x9a,0xd6,0x87,0x9f,0xbc,0xe5,0x21,0x35,0x32,0xf0,0xb5,0x02,0x1b,0xcd,0xcd,0xab,0xd2,0x5f,0xe9,0x41,0xfb,0xe7,0xfa,0x69,0x58,0x7a,0xd4,0x0a,0x5f,0xeb,0xec,0xc8,0x96,0xaa,0x1d,0x52,0x65,0xb6,0x2e,0xba,0x32,0x87,0x89,0xa3,0xa3,0x7a,0x5d,0x96,0x10,0x80,0x49,0xe0,0x63,0xea,0x68,0x16,0x5c,0xda,0xcf,0x8b,0x0d,0xa5,0x78,0x00,0x6f,0xc4,0xac,0xfc,0xde,0x38,0xb8,0x78,0xd9,0x01,0xa2,0x88,0xec,0x7c,0xcf,0xf7,0x35,0x3c,0xd5,0x00,0x8b,0x39,0x1d,0xac,0xd4,0x21,0x5d,0x64,0x01,0x1d,0xe2,0xca,0x9f,0xca,0x23,0xc8,0x89,0x79,0xcb,0x86,0xee,0x52,0x51,0x8c,0xb8,0x65,0x43,0x61,0x05,0xff,0xd1,0xd5,0xf6,0x82,0x6b,0xc3,0xd4,0x8d,0x9e,0xc0,0xcd,0xaf,0xe3,0x01,0x02,0x81,0x81,0x00,0xd3,0xf7,0x29,0xb7,0x32,0x69,0x6a,0xcc,0x81,0x4a,0x00,0x7b,0x27,0xbc,0x78,0x6c,0x11,0xc3,0xeb,0xee,0x55,0xb7,0x93,0x5d,0x4b,0xde,0x4d,0xc5,0x00,0x32,0xb4,0x96,0xa0,0x82,0xf0,0xcc,0xc3,0x74,0x72,0xc2,0xf8,0xc6,0x34,0x4c,0xbd,0x57,0x08,0x08,0xf3,0x81,0xc8,0x0d,0x0b,0x01,0x10,0x70,0x69,0x36,0x8a,0xd3,0x2a,0x2e,0x0b,0x0c,0xa4,0x17,0x4b,0x17,0xb1,0x5f,0x21,0x09,0x3f,0x91,0x71,0x58,0xcb,0x46,0xa9,0x6c,0x11,0x3f,0xa4,0x21,0xca,0x73,0xf3,0xc6,0xa5,0xe5,0xe9,0x3c,0x68,0xad,0x72,0x55,0xf5,0x9c,0x79,0x2b,0xbc,0x40,0xc1,0x35,0xfb,0x2d,0xfe,0x48,0x8b,0xdf,0x26,0x6a,0xba,0x6c,0x0c,0x38,0x73,0x3f,0x07,0xa5,0x0d,0x14,0xe0,0x55,0x7c,0xee,0x0c,0x51,0x02,0x81,0x81,0x00,0xc8,0x93,0x6e,0x11,0x79,0xef,0x25,0x8b,0xb3,0xce,0xa3,0x00,0xde,0xae,0x7d,0xa9,0x66,0x0c,0xff,0x7f,0x6f,0x0a,0x1c,0xce,0xea,0x74,0x32,0x79,0x21,0xa1,0xbf,0x35,0xf7,0x54,0x8e,0x32,0x74,0xf1,0x53,0x6d,0x0a,0xcc,0x29,0xb5,0x4b,0x2f,0x60,0x48,0xc4,0x12,0xce,0x5d,0xbf,0x66,0x87,0x35,0xab,0x0e,0xed,0x18,0x08,0xf6,0x67,0x8a,0x10,0x8d,0x7f,0x0b,0x64,0x45,0xb3,0xa6,0x9c,0xb8,0x97,0xa0,0x8e,0x31,0x90,0x38,0x8c,0x6c,0x55,0x9a,0x72,0xf1,0x2d,0x74,0xe6,0x2a,0x28,0x35,0x32,0xd2,0xea,0x20,0xd0,0xeb,0x98,0xe1,0x3c,0x5a,0xdd,0x90,0x67,0x87,0x96,0x38,0x86,0x8b,0xd6,0xba,0xde,0xa2,0x4f,0x93,0xd0,0x2b,0xc1,0x3b,0x8e,0xdc,0xf2,0x9b,0x72,0x0e,0x63,0x81,0x02,0x81,0x81,0x00,0xa0,0xae,0xc0,0x57,0xd9,0x06,0x62,0xe8,0xae,0xa5,0xfb,0xa1,0x60,0xa9,0x16,0x1c,0xce,0x86,0xd8,0x46,0x6f,0x82,0x5f,0x5b,0x1d,0x55,0x63,0x5d,0x10,0xde,0x5f,0x59,0xc4,0x4a,0x1e,0xe3,0x0a,0xd4,0x2e,0xdf,0x30,0x99,0x88,0x32,0x18,0x62,0x6e,0x7b,0xe2,0xa4,0xa2,0x93,0x88,0xa1,0x00,0xe6,0x46,0x4c,0x0b,0x87,0x32,0x75,0x7e,0xf8,0xd1,0x37,0xbb,0x3c,0x35,0xc0,0xa8,0x99,0x8b,0x2f,0xe5,0x5d,0xeb,0x93,0x20,0x82,0x04,0xbd,0x5a,0xde,0x26,0xcc,0x29,0xa3,0x90,0x45,0xae,0xbe,0x95,0x97,0xce,0x91,0xed,0x43,0xf5,0xf3,0x3a,0x9d,0x58,0x1a,0x64,0xd4,0x23,0xec,0xb8,0xe3,0xac,0x0a,0xc2,0x51,0x80,0x1e,0x16,0xf7,0x54,0x03,0x41,0xdc,0x6e,0x87,0x3f,0xd2,0x71,0x51,0x02,0x81,0x81,0x00,0xc0,0x2c,0xc1,0xa0,0xec,0xfb,0x3d,0x36,0x05,0x29,0x4e,0xf4,0x5e,0x67,0x40,0xef,0x35,0x5f,0xee,0xcc,0xe2,0x4f,0x06,0x1b,0x1a,0x78,0x0a,0x84,0x14,0xbd,0x2c,0xbf,0x40,0x22,0x3b,0xbc,0x0c,0x5f,0xcc,0x57,0x05,0xa9,0x67,0xac,0xd4,0x44,0xa8,0x34,0x0f,0xe9,0x14,0xe0,0xad,0xaa,0x46,0x92,0x1d,0x3b,0x47,0xdf,0x67,0x76,0xee,0xed,0x51,0xbe,0xc0,0xa2,0xe1,0x88,0x07,0x00,0xf3,0xd7,0xc9,0x2f,0x84,0x0d,0xe8,0xe8,0xc5,0x62,0x67,0x02,0x7c,0x6e,0xa7,0xe7,0x01,0x40,0x13,0x16,0x25,0x83,0xff,0xdf,0x41,0x4c,0xfa,0x45,0x93,0xd6,0x5b,0xc3,0x27,0xee,0xb8,0xf7,0x9f,0xb2,0x0c,0x23,0x2f,0x7c,0xc4,0x52,0x13,0x7a,0x58,0xc1,0xff,0xb7,0xa2,0x9b,0x5d,0x44,0x7c,0x01,0x02,0x81,0x81,0x00,0x98,0x80,0xda,0x67,0xb7,0x9a,0x27,0xd3,0x78,0x08,0x9e,0xac,0xd7,0x4c,0xbe,0xed,0x5f,0x85,0x07,0x71,0x93,0xd8,0xcf,0xe7,0x7c,0xf3,0x16,0xfd,0xc2,0x5d,0xea,0x09,0x4c,0xf3,0xea,0x54,0x41,0xb1,0x20,0x50,0x9a,0xad,0x4a,0xa7,0x4b,0x43,0x8c,0x71,0x8b,0x6d,0x75,0xb1,0xfa,0xb7,0xb9,0xbd,0x33,0x34,0x49,0x88,0x3e,0xac,0xc0,0x18,0x10,0x93,0x32,0xd2,0x8a,0xba,0x84,0xfa,0x56,0xcd,0x00,0xb0,0x62,0x35,0xad,0xde,0xd6,0x21,0x1c,0xa0,0xaa,0x88,0x41,0x01,0xe7,0x13,0x52,0xf5,0xf3,0x28,0xc2,0xfe,0x56,0xfc,0x29,0xeb,0xc7,0x90,0xd4,0x10,0x51,0xad,0x74,0xbf,0x45,0xd7,0xc1,0x31,0x7b,0x21,0xd2,0x2b,0x30,0xbd,0xb2,0xa6,0xea,0x72,0xf0,0xe7,0x0f,0xc8,0x43,0xfd];
        let ref_key = PrivateKey::from_pkcs8_der(RSA_2048_SP800_PRIV_DER).unwrap();
        assert_eq!(ref_key.validate(), Ok(()));

        let primes: Vec<_> = PrivateKeyParts::primes(&ref_key)
            .iter()
            .map(|p| p.as_ref().clone())
            .collect();
        let exp = PublicKeyParts::e(&ref_key);

        let key = PrivateKey::from_p_q(primes[0].clone(), primes[1].clone(), exp.clone())
            .expect("failed to import key from primes");
        assert_eq!(key.validate(), Ok(()));

        assert_eq!(PublicKeyParts::n(&key), PublicKeyParts::n(&ref_key));

        assert_eq!(PrivateKeyParts::dp(&key), PrivateKeyParts::dp(&ref_key));
        assert_eq!(PrivateKeyParts::dq(&key), PrivateKeyParts::dq(&ref_key));

        assert_eq!(PrivateKeyParts::d(&key), PrivateKeyParts::d(&ref_key));
    }

    #[test]
    #[cfg(feature = "hazmat")]
    fn test_from_components_with_large_exponent() {
        // Test that from_components_with_large_exponent accepts exponents outside normal bounds
        // while from_components would reject them

        use rand::rngs::ChaCha8Rng;
        use rand_core::SeedableRng;

        let mut rng = ChaCha8Rng::from_seed([42; 32]);

        // Use an exponent larger than the normal maximum (2^33 - 1)
        let large_e = BoxedUint::from((1u64 << 34) + 1); // 2^34 + 1 (odd number)

        // Generate a key with this large exponent
        let components =
            generate_multi_prime_key_with_exp(&mut rng, 2, 1024, large_e.clone()).unwrap();

        // Extract components
        let n = components.n.get().clone();
        let d = components.d;
        let primes = components.primes;

        // from_components should fail with PublicExponentTooLarge
        let result =
            PrivateKey::from_components(n.clone(), large_e.clone(), d.clone(), primes.clone());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), Error::PublicExponentTooLarge);

        // from_components_with_large_exponent should succeed
        let key_with_large_exp = PrivateKey::from_components_with_large_exponent(
            n.clone(),
            large_e.clone(),
            d.clone(),
            primes.clone(),
        );
        assert!(key_with_large_exp.is_ok());

        let key_with_large_exp = key_with_large_exp.unwrap();
        assert_eq!(PublicKeyParts::e(&key_with_large_exp), &large_e);
        assert_eq!(PublicKeyParts::n(&key_with_large_exp).as_ref(), &n);
        assert_eq!(PrivateKeyParts::d(&key_with_large_exp), &d);

        // Verify that the key is still cryptographically valid (de ≡ 1 mod λ(n))
        // by checking that validation with skip_exponent_size passes
        assert!(validate_skip_exponent_size(&key_with_large_exp).is_ok());
    }

    #[test]
    #[cfg(feature = "hazmat")]
    fn test_from_components_with_small_exponent() {
        // Test that from_components_with_large_exponent accepts exponents below normal minimum
        // (despite the name, it works for any non-standard exponent size)

        use rand::rngs::ChaCha8Rng;
        use rand_core::SeedableRng;

        let mut rng = ChaCha8Rng::from_seed([43; 32]);

        // Use an exponent smaller than the normal minimum (2)
        let small_e = BoxedUint::from(1u64); // This is odd, which is required

        // Generate a key with this small exponent
        let components =
            generate_multi_prime_key_with_exp(&mut rng, 2, 1024, small_e.clone()).unwrap();

        // Extract components
        let n = components.n.get().clone();
        let d = components.d;
        let primes = components.primes;

        // from_components should fail
        let result =
            PrivateKey::from_components(n.clone(), small_e.clone(), d.clone(), primes.clone());
        assert!(result.is_err());

        // from_components_with_large_exponent should succeed
        let key_with_small_exp = PrivateKey::from_components_with_large_exponent(
            n.clone(),
            small_e.clone(),
            d.clone(),
            primes,
        );
        assert!(key_with_small_exp.is_ok());

        let key_with_small_exp = key_with_small_exp.unwrap();
        assert_eq!(PublicKeyParts::e(&key_with_small_exp), &small_e);

        // Verify that the key is cryptographically valid
        assert!(validate_skip_exponent_size(&key_with_small_exp).is_ok());
    }

    /// Regression test for CVE-2026-21895 / GHSA-9c48-w39g-hm26.
    ///
    /// Loading a secret key whose prime factor is `1` must be rejected with
    /// `Error::InvalidPrime` rather than panicking via a divide-by-zero in
    /// the validation/precompute path.
    ///
    /// Adapted from the test added in upstream commit 2926c91bef (PR #624);
    /// the original used `num-bigint` `BigUint` types, this version uses
    /// `crypto-bigint` `BoxedUint` and goes through
    /// `from_components_with_large_exponent` so the small (out-of-range) `e`
    /// can be supplied verbatim from the original test.
    #[test]
    #[cfg(feature = "hazmat")]
    fn test_key_invalid_primes() {
        let e = PrivateKey::from_components_with_large_exponent(
            BoxedUint::from(239u64),
            BoxedUint::from(185u64),
            BoxedUint::from(0u64),
            vec![BoxedUint::from(1u64), BoxedUint::from(239u64)],
        )
        .unwrap_err();
        assert_eq!(e, Error::InvalidPrime);
    }
}

//! The `signature` module provides functionality for public, and private keys.
#![cfg(feature = "full")]

use crate::{pubkey::Pubkey, transaction::TransactionError};
use ed25519_dalek::Signer as DalekSigner;
use generic_array::{typenum::U64, GenericArray};
use hmac::Hmac;
use itertools::Itertools;
use rand::{rngs::OsRng, CryptoRng, RngCore};
use std::{
    borrow::{Borrow, Cow},
    convert::TryInto,
    error, fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    mem,
    path::Path,
    str::FromStr,
};
use thiserror::Error;

#[derive(Debug)]
pub struct Keypair(ed25519_dalek::Keypair);

impl Keypair {
    pub fn generate<R>(csprng: &mut R) -> Self
    where
        R: CryptoRng + RngCore,
    {
        Self(ed25519_dalek::Keypair::generate(csprng))
    }

    /// Return a new ED25519 keypair
    pub fn new() -> Self {
        let mut rng = OsRng::default();
        Self::generate(&mut rng)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ed25519_dalek::SignatureError> {
        ed25519_dalek::Keypair::from_bytes(bytes).map(Self)
    }

    pub fn to_bytes(&self) -> [u8; 64] {
        self.0.to_bytes()
    }

    pub fn from_base58_string(s: &str) -> Self {
        Self::from_bytes(&bs58::decode(s).into_vec().unwrap()).unwrap()
    }

    pub fn to_base58_string(&self) -> String {
        // Remove .iter() once we're rust 1.47+
        bs58::encode(&self.0.to_bytes().iter()).into_string()
    }

    pub fn secret(&self) -> &ed25519_dalek::SecretKey {
        &self.0.secret
    }
}

#[repr(transparent)]
#[derive(
    Serialize, Deserialize, Clone, Copy, Default, Eq, PartialEq, Ord, PartialOrd, Hash, AbiExample,
)]
pub struct Signature(GenericArray<u8, U64>);

impl crate::sanitize::Sanitize for Signature {}

impl Signature {
    pub fn new(signature_slice: &[u8]) -> Self {
        Self(GenericArray::clone_from_slice(&signature_slice))
    }

    pub(self) fn verify_verbose(
        &self,
        pubkey_bytes: &[u8],
        message_bytes: &[u8],
    ) -> Result<(), ed25519_dalek::SignatureError> {
        let publickey = ed25519_dalek::PublicKey::from_bytes(pubkey_bytes)?;
        let signature = self.0.as_slice().try_into()?;
        publickey.verify_strict(message_bytes, &signature)
    }

    pub fn verify(&self, pubkey_bytes: &[u8], message_bytes: &[u8]) -> bool {
        self.verify_verbose(pubkey_bytes, message_bytes).is_ok()
    }
}

pub trait Signable {
    fn sign(&mut self, keypair: &Keypair) {
        let signature = keypair.sign_message(self.signable_data().borrow());
        self.set_signature(signature);
    }
    fn verify(&self) -> bool {
        self.get_signature()
            .verify(&self.pubkey().as_ref(), self.signable_data().borrow())
    }

    fn pubkey(&self) -> Pubkey;
    fn signable_data(&self) -> Cow<[u8]>;
    fn get_signature(&self) -> Signature;
    fn set_signature(&mut self, signature: Signature);
}

impl AsRef<[u8]> for Signature {
    fn as_ref(&self) -> &[u8] {
        &self.0[..]
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", bs58::encode(self.0).into_string())
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", bs58::encode(self.0).into_string())
    }
}

impl Into<[u8; 64]> for Signature {
    fn into(self) -> [u8; 64] {
        <GenericArray<u8, U64> as Into<[u8; 64]>>::into(self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseSignatureError {
    #[error("string decoded to wrong size for signature")]
    WrongSize,
    #[error("failed to decode string to signature")]
    Invalid,
}

impl FromStr for Signature {
    type Err = ParseSignatureError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = bs58::decode(s)
            .into_vec()
            .map_err(|_| ParseSignatureError::Invalid)?;
        if bytes.len() != mem::size_of::<Signature>() {
            Err(ParseSignatureError::WrongSize)
        } else {
            Ok(Signature::new(&bytes))
        }
    }
}

pub trait Signer {
    fn pubkey(&self) -> Pubkey {
        self.try_pubkey().unwrap_or_default()
    }
    fn try_pubkey(&self) -> Result<Pubkey, SignerError>;
    fn sign_message(&self, message: &[u8]) -> Signature {
        self.try_sign_message(message).unwrap_or_default()
    }
    fn try_sign_message(&self, message: &[u8]) -> Result<Signature, SignerError>;
}

impl PartialEq for dyn Signer {
    fn eq(&self, other: &dyn Signer) -> bool {
        self.pubkey() == other.pubkey()
    }
}

impl std::fmt::Debug for dyn Signer {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(fmt, "Signer: {:?}", self.pubkey())
    }
}

/// Remove duplicates signers while preserving order. O(n²)
pub fn unique_signers(signers: Vec<&dyn Signer>) -> Vec<&dyn Signer> {
    signers.into_iter().unique_by(|s| s.pubkey()).collect()
}

impl Signer for Keypair {
    /// Return the public key for the given keypair
    fn pubkey(&self) -> Pubkey {
        Pubkey::new(self.0.public.as_ref())
    }

    fn try_pubkey(&self) -> Result<Pubkey, SignerError> {
        Ok(self.pubkey())
    }

    fn sign_message(&self, message: &[u8]) -> Signature {
        Signature::new(&self.0.sign(message).to_bytes())
    }

    fn try_sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        Ok(self.sign_message(message))
    }
}

impl<T> PartialEq<T> for Keypair
where
    T: Signer,
{
    fn eq(&self, other: &T) -> bool {
        self.pubkey() == other.pubkey()
    }
}

impl<T> From<T> for Box<dyn Signer>
where
    T: Signer + 'static,
{
    fn from(signer: T) -> Self {
        Box::new(signer)
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum SignerError {
    #[error("keypair-pubkey mismatch")]
    KeypairPubkeyMismatch,

    #[error("not enough signers")]
    NotEnoughSigners,

    #[error("transaction error")]
    TransactionError(#[from] TransactionError),

    #[error("custom error: {0}")]
    Custom(String),

    // Presigner-specific Errors
    #[error("presigner error")]
    PresignerError(#[from] PresignerError),

    // Remote Keypair-specific Errors
    #[error("connection error: {0}")]
    Connection(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("no device found")]
    NoDeviceFound,

    #[error("{0}")]
    Protocol(String),

    #[error("{0}")]
    UserCancel(String),
}

#[derive(Clone, Debug, Default)]
pub struct Presigner {
    pubkey: Pubkey,
    signature: Signature,
}

impl Presigner {
    pub fn new(pubkey: &Pubkey, signature: &Signature) -> Self {
        Self {
            pubkey: *pubkey,
            signature: *signature,
        }
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum PresignerError {
    #[error("pre-generated signature cannot verify data")]
    VerificationFailure,
}

impl Signer for Presigner {
    fn try_pubkey(&self) -> Result<Pubkey, SignerError> {
        Ok(self.pubkey)
    }

    fn try_sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        if self.signature.verify(self.pubkey.as_ref(), message) {
            Ok(self.signature)
        } else {
            Err(PresignerError::VerificationFailure.into())
        }
    }
}

impl<T> PartialEq<T> for Presigner
where
    T: Signer,
{
    fn eq(&self, other: &T) -> bool {
        self.pubkey() == other.pubkey()
    }
}

/// NullSigner - A `Signer` implementation that always produces `Signature::default()`.
/// Used as a placeholder for absentee signers whose 'Pubkey` is required to construct
/// the transaction
#[derive(Clone, Debug, Default)]
pub struct NullSigner {
    pubkey: Pubkey,
}

impl NullSigner {
    pub fn new(pubkey: &Pubkey) -> Self {
        Self { pubkey: *pubkey }
    }
}

impl Signer for NullSigner {
    fn try_pubkey(&self) -> Result<Pubkey, SignerError> {
        Ok(self.pubkey)
    }

    fn try_sign_message(&self, _message: &[u8]) -> Result<Signature, SignerError> {
        Ok(Signature::default())
    }
}

impl<T> PartialEq<T> for NullSigner
where
    T: Signer,
{
    fn eq(&self, other: &T) -> bool {
        self.pubkey == other.pubkey()
    }
}

pub fn read_keypair<R: Read>(reader: &mut R) -> Result<Keypair, Box<dyn error::Error>> {
    let bytes: Vec<u8> = serde_json::from_reader(reader)?;
    let dalek_keypair = ed25519_dalek::Keypair::from_bytes(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    Ok(Keypair(dalek_keypair))
}

pub fn read_keypair_file(path: &str) -> Result<Keypair, Box<dyn error::Error>> {
    assert!(path != "-");
    let mut file = File::open(path.to_string())?;
    read_keypair(&mut file)
}

pub fn write_keypair<W: Write>(
    keypair: &Keypair,
    writer: &mut W,
) -> Result<String, Box<dyn error::Error>> {
    let keypair_bytes = keypair.0.to_bytes();
    let serialized = serde_json::to_string(&keypair_bytes.to_vec())?;
    writer.write_all(&serialized.clone().into_bytes())?;
    Ok(serialized)
}

pub fn write_keypair_file(
    keypair: &Keypair,
    outfile: &str,
) -> Result<String, Box<dyn error::Error>> {
    assert!(outfile != "-");
    if let Some(outdir) = Path::new(outfile).parent() {
        fs::create_dir_all(outdir)?;
    }

    let mut f = {
        #[cfg(not(unix))]
        {
            OpenOptions::new()
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new().mode(0o600)
        }
    }
    .write(true)
    .truncate(true)
    .create(true)
    .open(outfile)?;

    write_keypair(keypair, &mut f)
}

pub fn keypair_from_seed(seed: &[u8]) -> Result<Keypair, Box<dyn error::Error>> {
    if seed.len() < ed25519_dalek::SECRET_KEY_LENGTH {
        return Err("Seed is too short".into());
    }
    let secret = ed25519_dalek::SecretKey::from_bytes(&seed[..ed25519_dalek::SECRET_KEY_LENGTH])
        .map_err(|e| e.to_string())?;
    let public = ed25519_dalek::PublicKey::from(&secret);
    let dalek_keypair = ed25519_dalek::Keypair { secret, public };
    Ok(Keypair(dalek_keypair))
}

pub fn keypair_from_seed_phrase_and_passphrase(
    seed_phrase: &str,
    passphrase: &str,
) -> Result<Keypair, Box<dyn error::Error>> {
    toml_config::package_config! {
        PBKDF2_ROUNDS: usize,
        PBKDF2_BYTES: usize,
    }

    let salt = format!("mnemonic{}", passphrase);

    let mut seed = vec![0u8; CFG.PBKDF2_BYTES];
    pbkdf2::pbkdf2::<Hmac<sha2::Sha512>>(
        seed_phrase.as_bytes(),
        salt.as_bytes(),
        CFG.PBKDF2_ROUNDS,
        &mut seed,
    );
    keypair_from_seed(&seed[..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use bip39::{Language, Mnemonic, MnemonicType, Seed};
    use std::mem;

    fn tmp_file_path(name: &str) -> String {
        use std::env;
        let out_dir = env::var("FARF_DIR").unwrap_or_else(|_| "farf".to_string());
        let keypair = Keypair::new();

        format!("{}/tmp/{}-{}", out_dir, name, keypair.pubkey())
    }

    #[test]
    fn test_write_keypair_file() {
        let outfile = tmp_file_path("test_write_keypair_file.json");
        let serialized_keypair = write_keypair_file(&Keypair::new(), &outfile).unwrap();
        let keypair_vec: Vec<u8> = serde_json::from_str(&serialized_keypair).unwrap();
        assert!(Path::new(&outfile).exists());
        assert_eq!(
            keypair_vec,
            read_keypair_file(&outfile).unwrap().0.to_bytes().to_vec()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                File::open(&outfile)
                    .expect("open")
                    .metadata()
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        assert_eq!(
            read_keypair_file(&outfile).unwrap().pubkey().as_ref().len(),
            mem::size_of::<Pubkey>()
        );
        fs::remove_file(&outfile).unwrap();
        assert!(!Path::new(&outfile).exists());
    }

    #[test]
    fn test_write_keypair_file_overwrite_ok() {
        let outfile = tmp_file_path("test_write_keypair_file_overwrite_ok.json");

        write_keypair_file(&Keypair::new(), &outfile).unwrap();
        write_keypair_file(&Keypair::new(), &outfile).unwrap();
    }

    #[test]
    fn test_write_keypair_file_truncate() {
        let outfile = tmp_file_path("test_write_keypair_file_truncate.json");

        write_keypair_file(&Keypair::new(), &outfile).unwrap();
        read_keypair_file(&outfile).unwrap();

        // Ensure outfile is truncated
        {
            let mut f = File::create(&outfile).unwrap();
            f.write_all(String::from_utf8([b'a'; 2048].to_vec()).unwrap().as_bytes())
                .unwrap();
        }
        write_keypair_file(&Keypair::new(), &outfile).unwrap();
        read_keypair_file(&outfile).unwrap();
    }

    #[test]
    fn test_keypair_from_seed() {
        let good_seed = vec![0; 32];
        assert!(keypair_from_seed(&good_seed).is_ok());

        let too_short_seed = vec![0; 31];
        assert!(keypair_from_seed(&too_short_seed).is_err());
    }

    #[test]
    fn test_signature_fromstr() {
        let signature = Keypair::new().sign_message(&[0u8]);

        let mut signature_base58_str = bs58::encode(signature).into_string();

        assert_eq!(signature_base58_str.parse::<Signature>(), Ok(signature));

        signature_base58_str.push_str(&bs58::encode(signature.0).into_string());
        assert_eq!(
            signature_base58_str.parse::<Signature>(),
            Err(ParseSignatureError::WrongSize)
        );

        signature_base58_str.truncate(signature_base58_str.len() / 2);
        assert_eq!(signature_base58_str.parse::<Signature>(), Ok(signature));

        signature_base58_str.truncate(signature_base58_str.len() / 2);
        assert_eq!(
            signature_base58_str.parse::<Signature>(),
            Err(ParseSignatureError::WrongSize)
        );

        let mut signature_base58_str = bs58::encode(signature.0).into_string();
        assert_eq!(signature_base58_str.parse::<Signature>(), Ok(signature));

        // throw some non-base58 stuff in there
        signature_base58_str.replace_range(..1, "I");
        assert_eq!(
            signature_base58_str.parse::<Signature>(),
            Err(ParseSignatureError::Invalid)
        );
    }

    #[test]
    fn test_keypair_from_seed_phrase_and_passphrase() {
        let mnemonic = Mnemonic::new(MnemonicType::Words12, Language::English);
        let passphrase = "42";
        let seed = Seed::new(&mnemonic, passphrase);
        let expected_keypair = keypair_from_seed(seed.as_bytes()).unwrap();
        let keypair =
            keypair_from_seed_phrase_and_passphrase(mnemonic.phrase(), passphrase).unwrap();
        assert_eq!(keypair.pubkey(), expected_keypair.pubkey());
    }

    #[test]
    fn test_keypair() {
        let keypair = keypair_from_seed(&[0u8; 32]).unwrap();
        let pubkey = keypair.pubkey();
        let data = [1u8];
        let sig = keypair.sign_message(&data);

        // Signer
        assert_eq!(keypair.try_pubkey().unwrap(), pubkey);
        assert_eq!(keypair.pubkey(), pubkey);
        assert_eq!(keypair.try_sign_message(&data).unwrap(), sig);
        assert_eq!(keypair.sign_message(&data), sig);

        // PartialEq
        let keypair2 = keypair_from_seed(&[0u8; 32]).unwrap();
        assert_eq!(keypair, keypair2);
    }

    #[test]
    fn test_presigner() {
        let keypair = keypair_from_seed(&[0u8; 32]).unwrap();
        let pubkey = keypair.pubkey();
        let data = [1u8];
        let sig = keypair.sign_message(&data);

        // Signer
        let presigner = Presigner::new(&pubkey, &sig);
        assert_eq!(presigner.try_pubkey().unwrap(), pubkey);
        assert_eq!(presigner.pubkey(), pubkey);
        assert_eq!(presigner.try_sign_message(&data).unwrap(), sig);
        assert_eq!(presigner.sign_message(&data), sig);
        let bad_data = [2u8];
        assert!(presigner.try_sign_message(&bad_data).is_err());
        assert_eq!(presigner.sign_message(&bad_data), Signature::default());

        // PartialEq
        assert_eq!(presigner, keypair);
        assert_eq!(keypair, presigner);
        let presigner2 = Presigner::new(&pubkey, &sig);
        assert_eq!(presigner, presigner2);
    }

    fn pubkeys(signers: &[&dyn Signer]) -> Vec<Pubkey> {
        signers.iter().map(|x| x.pubkey()).collect()
    }

    #[test]
    fn test_unique_signers() {
        let alice = Keypair::new();
        let bob = Keypair::new();
        assert_eq!(
            pubkeys(&unique_signers(vec![&alice, &bob, &alice])),
            pubkeys(&[&alice, &bob])
        );
    }

    #[test]
    fn test_off_curve_pubkey_verify_fails() {
        // Golden point off the ed25519 curve
        let off_curve_bytes = bs58::decode("9z5nJyQar1FUxVJxpBXzon6kHehbomeYiDaLi9WAMhCq")
            .into_vec()
            .unwrap();

        // Confirm golden's off-curvedness
        let mut off_curve_bits = [0u8; 32];
        off_curve_bits.copy_from_slice(&off_curve_bytes);
        let off_curve_point = curve25519_dalek::edwards::CompressedEdwardsY(off_curve_bits);
        assert_eq!(off_curve_point.decompress(), None);

        let pubkey = Pubkey::new(&off_curve_bytes);
        let signature = Signature::default();
        // Unfortunately, ed25519-dalek doesn't surface the internal error types that we'd ideally
        // `source()` out of the `SignatureError` returned by `verify_strict()`.  So the best we
        // can do is `is_err()` here.
        assert!(signature.verify_verbose(pubkey.as_ref(), &[0u8]).is_err());
    }
}

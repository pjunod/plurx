//! Envelope encryption for the one credential plurx cannot store as a hash.
//!
//! Every other credential plurx holds — passwords, login tokens, API keys,
//! offline lease capabilities — is verified, so a hash is enough. Trakt is the
//! exception: plurx must replay the upstream OAuth access and refresh tokens to
//! make outbound calls, so it has to be able to recover them.
//!
//! That matters for clustering. [`CLUSTERING-PLAN.md`] §3.2 requires that
//! secrets stay local: a replicated row may carry ciphertext, a key id, and
//! non-secret metadata, but the unwrapping key is node-local file material and
//! never a durable row. Replicating a cleartext bearer token would copy it into
//! every voter database, raft WAL, snapshot, and backup, where deleting the row
//! cannot erase the historical log entries.
//!
//! So the durable column holds a [`SealedSecret`] — an opaque envelope string —
//! and the cleartext exists only between [`CredentialKey::seal_trakt`] and
//! [`CredentialKey::open_trakt`], inside one node's process.
//!
//! [`CLUSTERING-PLAN.md`]: ../../../docs/CLUSTERING-PLAN.md

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

/// Default file name for the node-local wrapping key, beside `node.id`.
pub const CREDENTIAL_KEY_FILENAME: &str = "credentials.key";

/// Envelope prefix. Present on every wrapped value and on nothing a Trakt
/// server ever issues, so it also decides "is this row already migrated?".
const ENVELOPE_PREFIX: &str = "plxenc";
/// Envelope version. Bumping it changes the wire format, not the key.
const ENVELOPE_VERSION: &str = "v1";
/// XChaCha20-Poly1305 nonce width. 24 random bytes is wide enough that a
/// per-key counter is unnecessary — which matters because several voters may
/// seal under the same key without coordinating.
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
/// Key ids are short so the envelope stays readable in a row dump; they are
/// public, and identify which key opens this value, not the key itself.
const KEY_ID_LEN: usize = 8;

/// What can go wrong wrapping, unwrapping, or loading key material.
#[derive(Debug, Error)]
pub enum SecretError {
    #[error("credential key file {path}: {message}")]
    KeyFile { path: PathBuf, message: String },

    #[error(
        "credential key file {path} is missing, but {rows} stored Trakt credential(s) are \
         encrypted under it; restore the key file or unlink Trakt — refusing to start without it"
    )]
    KeyMissingForWrappedRows { path: PathBuf, rows: usize },

    #[error(
        "credential key file {path} holds key {held}, but {rows} stored Trakt credential(s) are \
         sealed under key {wanted}; this is not the key that sealed this database — restore the \
         matching key file or unlink Trakt, rather than starting with a key that opens nothing"
    )]
    WrongKeyForStoredRows {
        path: PathBuf,
        held: String,
        wanted: String,
        rows: usize,
    },

    #[error(
        "credential key file {path} is mode {mode}; a node-local credential key must be readable \
         only by its owner — run `chmod 600` on it and restart"
    )]
    KeyFilePermissions { path: PathBuf, mode: String },

    #[error("stored credential is not an encrypted envelope; it must be migrated before use")]
    NotWrapped,

    #[error("stored credential envelope is malformed: {0}")]
    Malformed(String),

    #[error(
        "stored credential is sealed under key {wanted}, but this node holds key {held}; \
         the wrong credential key file is configured"
    )]
    WrongKey { wanted: String, held: String },

    #[error("stored credential failed authenticated decryption")]
    Undecryptable,
}

/// Cleartext that must not be logged, printed, or serialized by accident.
///
/// `Debug` and `Display` redact. There is no `Serialize`: a bearer token has no
/// business in a JSON body plurx builds, and the compiler is a better guard
/// than a review checklist.
pub struct Secret(Zeroizing<String>);

impl Secret {
    fn new(value: String) -> Self {
        Secret(Zeroizing::new(value))
    }

    /// Adopt cleartext that arrived from outside plurx — an OAuth response
    /// body, say — so it gets the same redaction and zeroizing as a credential
    /// that came back out of the store.
    pub fn from_cleartext(value: impl Into<String>) -> Self {
        Secret::new(value.into())
    }

    /// The cleartext, for the one thing it is for: an outbound call.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(redacted)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("redacted")
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Secret {}

/// A wrapped credential exactly as it sits in a durable row.
///
/// This is the only form that crosses the `Store` boundary. Outside this crate
/// the sole way to obtain one is [`CredentialKey::seal_trakt`], so no caller
/// can hand a store a string it invented:
///
/// ```compile_fail
/// # use plurx_core::secrets::SealedSecret;
/// // `from_stored` is crate-private on purpose: adopting an arbitrary string
/// // is a thing only the row reader and the upgrade pass may do.
/// let smuggled = SealedSecret::from_stored("plain-bearer-token");
/// ```
///
/// Inside the crate it *is* constructible from an arbitrary stored string,
/// because a pre-encryption install has cleartext in that column and
/// [`is_wrapped`](Self::is_wrapped) is how the upgrade path finds those rows.
/// Durable writes therefore do not trust the type alone — they go through
/// [`to_persist`](Self::to_persist), which refuses an unwrapped value. Reading
/// one back out is not possible without the key: [`CredentialKey::open_trakt`]
/// refuses anything unwrapped.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedSecret(String);

impl SealedSecret {
    /// Adopt a value read from a durable row, wrapped or not.
    ///
    /// Crate-private: the two callers that need it are the row reader — which
    /// cannot assume a pre-encryption install has been upgraded yet — and the
    /// upgrade pass itself. Everything else seals through the key.
    pub(crate) fn from_stored(stored: impl Into<String>) -> Self {
        SealedSecret(stored.into())
    }

    /// The exact text as it sits in the row, wrapped or not.
    ///
    /// This is the compare-and-set operand — two nodes racing a token rotation
    /// compare stored envelopes, never cleartext — and the value the upgrade
    /// pass inspects. It is deliberately *not* what a durable write uses; see
    /// [`to_persist`](Self::to_persist).
    pub fn as_stored(&self) -> &str {
        &self.0
    }

    /// The text a durable write may persist — an envelope, or an error.
    ///
    /// Durable writes go through this rather than
    /// [`as_stored`](Self::as_stored) because `as_stored` also serves two
    /// callers that legitimately hold a pre-encryption value. Checking at the
    /// write is what makes "a durable Trakt row holds ciphertext" a property
    /// the store enforces, rather than a property of every caller having
    /// remembered to seal first.
    pub fn to_persist(&self) -> Result<&str, SecretError> {
        if !self.is_wrapped() {
            return Err(SecretError::NotWrapped);
        }
        Ok(&self.0)
    }

    /// True once this value is an envelope this build can parse.
    pub fn is_wrapped(&self) -> bool {
        self.parse().is_ok()
    }

    /// True when the value carries the envelope marker at all, parseable or
    /// not.
    ///
    /// This — not [`is_wrapped`](Self::is_wrapped) — is the question the
    /// startup key check asks. A truncated or future-version envelope is still
    /// not a credential anyone may use, so treating it as "already encrypted"
    /// keeps a damaged row on the refusal path instead of the re-seal path,
    /// where sealing the envelope text itself would destroy the original.
    pub fn looks_wrapped(&self) -> bool {
        self.0.starts_with(&format!("{ENVELOPE_PREFIX}:"))
    }

    /// The key id an envelope names, for diagnostics that must not reveal the
    /// ciphertext. `None` for a value that is not an envelope at all.
    ///
    /// Deliberately laxer than [`is_wrapped`](Self::is_wrapped), for the same
    /// reason [`looks_wrapped`](Self::looks_wrapped) is: it reads the id
    /// without validating the body. A row naming a key this node does not hold
    /// is the wrong key file whether or not its ciphertext survived intact, and
    /// treating a damaged envelope as "no opinion about the key" would let a
    /// replaced key file boot.
    pub fn key_id(&self) -> Option<String> {
        let mut parts = self.0.split(':');
        if parts.next()? != ENVELOPE_PREFIX {
            return None;
        }
        // A future envelope version still names its key id in this field.
        parts.next()?;
        let key_id = parts.next()?;
        (key_id.len() == KEY_ID_LEN && key_id.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| key_id.to_owned())
    }

    fn parse(&self) -> Result<ParsedEnvelope, SecretError> {
        let mut parts = self.0.split(':');
        let prefix = parts.next().unwrap_or_default();
        if prefix != ENVELOPE_PREFIX {
            return Err(SecretError::NotWrapped);
        }
        let version = parts.next().ok_or(SecretError::NotWrapped)?;
        if version != ENVELOPE_VERSION {
            return Err(SecretError::Malformed(format!(
                "unsupported envelope version `{version}`"
            )));
        }
        let key_id = parts
            .next()
            .ok_or_else(|| SecretError::Malformed("envelope is missing its key id".to_owned()))?;
        let body = parts
            .next()
            .ok_or_else(|| SecretError::Malformed("envelope is missing its body".to_owned()))?;
        if parts.next().is_some() {
            return Err(SecretError::Malformed(
                "envelope has trailing fields".to_owned(),
            ));
        }
        if key_id.len() != KEY_ID_LEN || !key_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SecretError::Malformed(
                "envelope key id is not a hex key id".to_owned(),
            ));
        }

        let raw = hex_decode(body)
            .ok_or_else(|| SecretError::Malformed("envelope body is not hex".to_owned()))?;
        if raw.len() <= NONCE_LEN {
            return Err(SecretError::Malformed(
                "envelope body is shorter than its nonce".to_owned(),
            ));
        }
        let (nonce, ciphertext) = raw.split_at(NONCE_LEN);
        Ok(ParsedEnvelope {
            key_id: key_id.to_owned(),
            nonce: nonce.to_vec(),
            ciphertext: ciphertext.to_vec(),
        })
    }
}

/// Redacted on purpose. The envelope is not readable without the key, but a
/// ciphertext in a log is still a copy of a household's credential sitting
/// somewhere nobody is guarding.
impl fmt::Debug for SealedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.key_id(), self.is_wrapped()) {
            (Some(id), true) => write!(f, "SealedSecret(wrapped, key {id})"),
            (Some(id), false) => write!(f, "SealedSecret(damaged envelope, key {id})"),
            (None, _) => f.write_str("SealedSecret(unwrapped)"),
        }
    }
}

struct ParsedEnvelope {
    key_id: String,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

/// Node-local key material that wraps durable credentials.
///
/// Held in memory for the process lifetime and zeroized on drop. It is loaded
/// from a mode-`0600` file beside `node.id` and is never written to a durable
/// row, a setting, a log line, or an API response.
pub struct CredentialKey {
    id: String,
    key: [u8; KEY_LEN],
}

impl CredentialKey {
    /// Build a key from raw material. The id is derived from the key so two
    /// nodes given the same key file agree on the id without coordinating.
    pub fn from_bytes(key: [u8; KEY_LEN]) -> Self {
        let id = derive_key_id(&key);
        CredentialKey { id, key }
    }

    /// Public identifier of the key, safe to log and to store in an envelope.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Mint fresh key material that is never written anywhere.
    ///
    /// For callers with no data directory to resolve a key file from. Anything
    /// sealed under it becomes unreadable when the process ends, which is the
    /// correct behaviour for a key nobody persisted.
    pub fn generate() -> Self {
        let mut key = [0u8; KEY_LEN];
        // A failure here means the OS has no randomness at all; there is no
        // weaker key worth substituting.
        getrandom::getrandom(&mut key).expect("operating-system randomness");
        Self::from_bytes(key)
    }

    /// Read the key file at `path`, refusing one that anybody but its owner can
    /// read.
    ///
    /// The mode is checked on every load, not only on the file this node minted
    /// itself. A key restored from a backup, copied to a second voter, or
    /// written by hand is precisely the one most likely to arrive group- or
    /// world-readable, and it is the one `docs/SECURITY.md` promises is mode
    /// `0600`. Refusing names a one-command fix; accepting silently downgrades
    /// the guarantee the whole envelope rests on.
    pub fn load(path: &Path) -> Result<Self, SecretError> {
        let key_file = |message: String| SecretError::KeyFile {
            path: path.to_owned(),
            message,
        };
        let mut file =
            File::open(path).map_err(|error| key_file(format!("cannot be read: {error}")))?;

        #[cfg(unix)]
        {
            let mode = file
                .metadata()
                .map_err(|error| key_file(format!("has no readable metadata: {error}")))?
                .permissions()
                .mode()
                & 0o777;
            if mode & 0o077 != 0 {
                return Err(SecretError::KeyFilePermissions {
                    path: path.to_owned(),
                    mode: format!("{mode:04o}"),
                });
            }
        }

        let mut raw = Zeroizing::new(String::new());
        file.read_to_string(&mut raw)
            .map_err(|error| key_file(format!("cannot be read: {error}")))?;
        Self::parse_key_file(path, &raw)
    }

    /// Read the key file, creating it if absent.
    ///
    /// Creation is a caller's decision, not a fallback: minting a fresh key
    /// beside rows sealed under a key that has gone missing would strand those
    /// rows, so [`open_credential_key`] only permits creation when no wrapped
    /// row exists.
    pub fn load_or_create(path: &Path) -> Result<Self, SecretError> {
        match Self::load(path) {
            Ok(key) => Ok(key),
            Err(SecretError::KeyFile { .. }) if !path.exists() => {
                create_key_file(path)?;
                Self::load(path)
            }
            Err(error) => Err(error),
        }
    }

    fn parse_key_file(path: &Path, raw: &str) -> Result<Self, SecretError> {
        let trimmed = raw.trim();
        let mut bytes = hex_decode(trimmed).ok_or_else(|| SecretError::KeyFile {
            path: path.to_owned(),
            message: "is not hex-encoded key material".to_owned(),
        })?;
        if bytes.len() != KEY_LEN {
            bytes.zeroize();
            return Err(SecretError::KeyFile {
                path: path.to_owned(),
                message: format!("must hold {KEY_LEN} bytes of hex key material"),
            });
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&bytes);
        bytes.zeroize();
        Ok(Self::from_bytes(key))
    }

    /// Wrap a Trakt bearer credential for `user_id`.
    ///
    /// The user id is authenticated additional data, not ciphertext: moving a
    /// sealed row to another user's `user_id` makes it fail to open rather than
    /// silently handing one household member another's Trakt account.
    pub fn seal_trakt(&self, user_id: i64, cleartext: &str) -> Result<SealedSecret, SecretError> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|error| {
            SecretError::Malformed(format!("no operating-system randomness: {error}"))
        })?;
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: cleartext.as_bytes(),
                    aad: trakt_aad(user_id).as_bytes(),
                },
            )
            .map_err(|_| SecretError::Undecryptable)?;

        let mut body = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        body.extend_from_slice(&nonce);
        body.extend_from_slice(&ciphertext);
        Ok(SealedSecret(format!(
            "{ENVELOPE_PREFIX}:{ENVELOPE_VERSION}:{}:{}",
            self.id,
            hex_encode(&body)
        )))
    }

    /// Unwrap a Trakt bearer credential for `user_id`.
    ///
    /// There is no cleartext path out of this function. A row that was never
    /// migrated returns [`SecretError::NotWrapped`] rather than its own
    /// contents, so a half-finished upgrade fails loudly instead of quietly
    /// continuing to use a plaintext secret.
    pub fn open_trakt(&self, user_id: i64, sealed: &SealedSecret) -> Result<Secret, SecretError> {
        let parsed = sealed.parse()?;
        if parsed.key_id != self.id {
            return Err(SecretError::WrongKey {
                wanted: parsed.key_id,
                held: self.id.clone(),
            });
        }
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let mut cleartext = cipher
            .decrypt(
                XNonce::from_slice(&parsed.nonce),
                Payload {
                    msg: &parsed.ciphertext,
                    aad: trakt_aad(user_id).as_bytes(),
                },
            )
            .map_err(|_| SecretError::Undecryptable)?;
        let value = String::from_utf8(cleartext.clone()).map_err(|_| {
            SecretError::Malformed("decrypted credential is not valid UTF-8".to_owned())
        });
        cleartext.zeroize();
        Ok(Secret::new(value?))
    }
}

impl fmt::Debug for CredentialKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CredentialKey({})", self.id)
    }
}

impl Drop for CredentialKey {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

fn trakt_aad(user_id: i64) -> String {
    format!("plurx.trakt.{ENVELOPE_VERSION}.user.{user_id}")
}

/// A key id that is a one-way function of the key, so it identifies the key
/// without narrowing a search for it.
fn derive_key_id(key: &[u8; KEY_LEN]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"plurx.credential-key-id.v1");
    hasher.update(key);
    hex_encode(&hasher.finalize()[..KEY_ID_LEN / 2])
}

/// What startup can learn about the sealed rows already on disk *without*
/// holding the key: how many there are, and which keys they name.
///
/// Both halves decide something. The count separates a first boot, which may
/// mint a key, from an install whose key file went missing, which may not. The
/// key ids separate the right key file from a replaced one — a wrong key opens
/// nothing, and discovering that at the first outbound Trakt call rather than
/// at startup means a household loses sync with no error anybody is looking at.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SealedRowCensus {
    rows: usize,
    key_ids: BTreeSet<String>,
}

impl SealedRowCensus {
    /// Record one durable row's bearer pair.
    ///
    /// A row counts once even when both columns are sealed, and counts at all
    /// when only one is: a boot killed between the two columns leaves a mixed
    /// row that still needs the key that sealed the first half.
    pub fn observe_row(&mut self, access: &SealedSecret, refresh: &SealedSecret) {
        let mut sealed = false;
        for value in [access, refresh] {
            if !value.looks_wrapped() {
                continue;
            }
            sealed = true;
            if let Some(id) = value.key_id() {
                self.key_ids.insert(id);
            }
        }
        if sealed {
            self.rows += 1;
        }
    }

    /// How many durable rows hold at least one sealed column.
    pub fn sealed_rows(&self) -> usize {
        self.rows
    }

    /// Refuse a key that cannot open the rows already on disk.
    ///
    /// The test is on key ids rather than on trial decryption: an id is a
    /// one-way function of the key, is public, and is the one thing about a key
    /// that is safe to print in an error an operator has to read. A row damaged
    /// past naming any key id is not a statement about the key file, so it stays
    /// a row-local failure at use rather than a refusal that stops the whole
    /// server over one corrupt column.
    fn verify(&self, path: &Path, key: &CredentialKey) -> Result<(), SecretError> {
        // BTreeSet iteration is already ordered, so the message is stable.
        let foreign: Vec<&str> = self
            .key_ids
            .iter()
            .map(String::as_str)
            .filter(|id| *id != key.id())
            .collect();
        if foreign.is_empty() {
            return Ok(());
        }
        Err(SecretError::WrongKeyForStoredRows {
            path: path.to_owned(),
            held: key.id().to_owned(),
            wanted: foreign.join(", "),
            rows: self.rows,
        })
    }
}

/// Resolve the node-local key for a store whose sealed rows `census` describes.
///
/// The rule is one sentence: never continue with a key that cannot open what is
/// already on disk, and never fall back to cleartext. Both a missing key file
/// and a replaced one are refusals, because every silent alternative — minting a
/// fresh key, keeping a key that opens nothing, reading the rows as plaintext —
/// ends with a household either locked out without being told or newly exposed
/// without being told.
pub fn open_credential_key(
    path: &Path,
    census: &SealedRowCensus,
) -> Result<CredentialKey, SecretError> {
    if !path.exists() {
        if census.sealed_rows() > 0 {
            return Err(SecretError::KeyMissingForWrappedRows {
                path: path.to_owned(),
                rows: census.sealed_rows(),
            });
        }
        return CredentialKey::load_or_create(path);
    }
    let key = CredentialKey::load(path)?;
    census.verify(path, &key)?;
    Ok(key)
}

/// Write fresh key material to `path` with owner-only permissions, fsynced, and
/// never clobbering a key another process won the race to create.
fn create_key_file(path: &Path) -> Result<(), SecretError> {
    let mut key = [0u8; KEY_LEN];
    getrandom::getrandom(&mut key).map_err(|error| SecretError::KeyFile {
        path: path.to_owned(),
        message: format!("no operating-system randomness: {error}"),
    })?;
    let mut encoded = Zeroizing::new(hex_encode(&key));
    key.zeroize();
    encoded.push('\n');

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    let mut file = match options.open(path) {
        Ok(file) => file,
        // Another process created it first; its key is as good as ours.
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => {
            return Err(SecretError::KeyFile {
                path: path.to_owned(),
                message: format!("cannot be created: {error}"),
            })
        }
    };
    file.write_all(encoded.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|error| SecretError::KeyFile {
            path: path.to_owned(),
            message: format!("cannot be written: {error}"),
        })
}

fn hex_encode(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() {
        return None;
    }
    hex::decode(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key(seed: u8) -> CredentialKey {
        CredentialKey::from_bytes([seed; KEY_LEN])
    }

    /// The census a store with `rows` sealed rows under `key` would produce.
    fn census_sealed_under(key: &CredentialKey, rows: usize) -> SealedRowCensus {
        let mut census = SealedRowCensus::default();
        for user_id in 0..rows as i64 {
            census.observe_row(
                &key.seal_trakt(user_id, "access").expect("seal access"),
                &key.seal_trakt(user_id, "refresh").expect("seal refresh"),
            );
        }
        census
    }

    #[test]
    fn a_sealed_credential_never_contains_its_cleartext() {
        let key = test_key(7);
        let sealed = key.seal_trakt(42, "trakt-bearer-cleartext").expect("seal");

        assert!(!sealed.as_stored().contains("trakt-bearer-cleartext"));
        assert!(sealed.as_stored().starts_with("plxenc:v1:"));
        assert!(sealed.is_wrapped());
        assert_eq!(
            key.open_trakt(42, &sealed).expect("open").expose(),
            "trakt-bearer-cleartext"
        );
    }

    #[test]
    fn sealing_the_same_credential_twice_produces_different_ciphertext() {
        let key = test_key(9);
        let first = key.seal_trakt(1, "same-token").expect("seal");
        let second = key.seal_trakt(1, "same-token").expect("seal");
        assert_ne!(first.as_stored(), second.as_stored());
    }

    #[test]
    fn a_sealed_credential_does_not_open_for_another_user() {
        let key = test_key(3);
        let sealed = key.seal_trakt(1, "user-one-token").expect("seal");
        assert!(matches!(
            key.open_trakt(2, &sealed),
            Err(SecretError::Undecryptable)
        ));
    }

    #[test]
    fn another_nodes_key_cannot_open_a_copied_row() {
        let mine = test_key(1);
        let theirs = test_key(2);
        let sealed = mine.seal_trakt(1, "household-token").expect("seal");
        assert!(matches!(
            theirs.open_trakt(1, &sealed),
            Err(SecretError::WrongKey { .. })
        ));
    }

    #[test]
    fn a_tampered_envelope_fails_instead_of_decrypting() {
        let key = test_key(5);
        let sealed = key.seal_trakt(1, "token").expect("seal");
        let mut flipped: Vec<char> = sealed.as_stored().chars().collect();
        let last = flipped.len() - 1;
        flipped[last] = if flipped[last] == '0' { '1' } else { '0' };
        let tampered = SealedSecret::from_stored(flipped.into_iter().collect::<String>());
        assert!(matches!(
            key.open_trakt(1, &tampered),
            Err(SecretError::Undecryptable)
        ));
    }

    #[test]
    fn a_legacy_cleartext_row_is_refused_rather_than_returned() {
        let key = test_key(4);
        let legacy = SealedSecret::from_stored("plain-bearer-token");
        assert!(!legacy.is_wrapped());
        assert!(matches!(
            key.open_trakt(1, &legacy),
            Err(SecretError::NotWrapped)
        ));
    }

    #[test]
    fn a_malformed_envelope_is_named_rather_than_treated_as_cleartext() {
        let key = test_key(4);
        for bad in [
            "plxenc:v2:aabbccdd:0011",
            "plxenc:v1:nothex!!:0011",
            "plxenc:v1:aabbccdd:zz",
            "plxenc:v1:aabbccdd",
            "plxenc:v1:aabbccdd:00:extra",
        ] {
            let sealed = SealedSecret::from_stored(bad);
            assert!(!sealed.is_wrapped(), "{bad} should not parse");
            assert!(key.open_trakt(1, &sealed).is_err(), "{bad} must not open");
        }
    }

    #[test]
    fn neither_the_key_nor_a_secret_prints_its_material() {
        let key = test_key(6);
        let sealed = key.seal_trakt(1, "super-secret-bearer").expect("seal");
        let secret = key.open_trakt(1, &sealed).expect("open");

        assert!(!format!("{key:?}").contains("0606"));
        assert_eq!(format!("{secret:?}"), "Secret(redacted)");
        assert_eq!(format!("{secret}"), "redacted");
        assert!(!format!("{sealed:?}").contains(sealed.as_stored()));
    }

    #[test]
    fn a_key_file_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CREDENTIAL_KEY_FILENAME);

        let created = CredentialKey::load_or_create(&path).expect("create");
        let reopened = CredentialKey::load_or_create(&path).expect("reopen");
        assert_eq!(created.id(), reopened.id());

        let sealed = created.seal_trakt(1, "token").expect("seal");
        assert_eq!(
            reopened.open_trakt(1, &sealed).expect("open").expose(),
            "token"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "key file must not be group- or world-readable");
        }
    }

    #[test]
    fn a_missing_key_beside_wrapped_rows_refuses_to_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CREDENTIAL_KEY_FILENAME);
        let census = census_sealed_under(&test_key(1), 2);

        let error = open_credential_key(&path, &census).expect_err("must refuse");
        assert!(matches!(
            error,
            SecretError::KeyMissingForWrappedRows { rows: 2, .. }
        ));
        assert!(!path.exists(), "a refusal must not mint a replacement key");
    }

    #[test]
    fn a_missing_key_with_nothing_wrapped_yet_is_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CREDENTIAL_KEY_FILENAME);

        let key = open_credential_key(&path, &SealedRowCensus::default()).expect("create");
        assert!(path.exists());
        assert_eq!(key.id().len(), KEY_ID_LEN);
    }

    /// The wrong key present is the same failure as no key at all wearing a
    /// disguise: it opens nothing. Loading it anyway defers the discovery to the
    /// first outbound Trakt call, where it reads as "not linked".
    #[test]
    fn a_present_key_that_opens_nothing_is_refused_like_a_missing_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CREDENTIAL_KEY_FILENAME);
        let replacement = open_credential_key(&path, &SealedRowCensus::default()).expect("mint");
        let census = census_sealed_under(&test_key(0x5a), 3);

        let error = open_credential_key(&path, &census).expect_err("must refuse");
        let SecretError::WrongKeyForStoredRows {
            held, wanted, rows, ..
        } = &error
        else {
            panic!("expected a wrong-key refusal, got {error}");
        };
        assert_eq!(held, replacement.id());
        assert_eq!(wanted, test_key(0x5a).id());
        assert_eq!(*rows, 3);
        // Public ids only: the refusal an operator reads names which key is
        // wanted, never any key material.
        assert!(!error.to_string().contains(&hex_encode(&[0x5a; KEY_LEN])));
    }

    /// A damaged envelope is one bad row, not a statement about the key file.
    /// Refusing to boot the whole server over it would be the wrong trade — and
    /// it still cannot be read as cleartext, because `open_trakt` refuses it.
    #[test]
    fn a_row_too_damaged_to_name_a_key_does_not_condemn_the_key_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CREDENTIAL_KEY_FILENAME);
        let mut census = SealedRowCensus::default();
        census.observe_row(
            &SealedSecret::from_stored("plxenc:v1:tru"),
            &SealedSecret::from_stored("plxenc:v1:tru"),
        );

        assert_eq!(
            census.sealed_rows(),
            1,
            "it still needs a key file to exist"
        );
        let key = open_credential_key(&path, &SealedRowCensus::default()).expect("mint");
        open_credential_key(&path, &census).expect("a damaged row must not block startup");
        assert!(matches!(
            key.open_trakt(1, &SealedSecret::from_stored("plxenc:v1:tru")),
            Err(SecretError::Malformed(_))
        ));
    }

    /// A key sealed by one column and a cleartext other column is a boot killed
    /// mid-migration; it still names the key that did the first half.
    #[test]
    fn a_half_sealed_row_counts_and_names_its_key() {
        let key = test_key(0x31);
        let mut census = SealedRowCensus::default();
        census.observe_row(
            &key.seal_trakt(7, "access").expect("seal"),
            &SealedSecret::from_stored("still-cleartext-refresh"),
        );

        assert_eq!(census.sealed_rows(), 1);
        assert!(census.verify(Path::new("/key"), &key).is_ok());
        assert!(census.verify(Path::new("/key"), &test_key(0x32)).is_err());
    }

    /// Write a key file the way an operator restoring one would, owner-only, so
    /// the test that follows is about the file's *contents*.
    fn write_owner_only(path: &Path, contents: &str) {
        std::fs::write(path, contents).expect("write");
        #[cfg(unix)]
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("owner-only");
    }

    #[test]
    fn a_corrupt_key_file_is_refused_rather_than_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CREDENTIAL_KEY_FILENAME);
        write_owner_only(&path, "not-hex\n");

        assert!(matches!(
            open_credential_key(&path, &census_sealed_under(&test_key(1), 1)),
            Err(SecretError::KeyFile { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "not-hex\n",
            "a refusal must leave the operator's file alone"
        );
    }

    #[test]
    fn a_short_key_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CREDENTIAL_KEY_FILENAME);
        write_owner_only(&path, "aabbcc\n");

        assert!(matches!(
            CredentialKey::load(&path),
            Err(SecretError::KeyFile { .. })
        ));
    }

    /// `docs/SECURITY.md` promises the key is owner-only. A restored backup or a
    /// hand-written key is exactly the one that arrives mode `0644`, so the
    /// promise has to be checked on load, not only on the file we minted.
    #[cfg(unix)]
    #[test]
    fn a_key_file_anyone_can_read_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CREDENTIAL_KEY_FILENAME);
        let minted = open_credential_key(&path, &SealedRowCensus::default()).expect("mint");
        let before = std::fs::read_to_string(&path).expect("read");

        for exposed in [0o644, 0o640, 0o604, 0o666] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(exposed))
                .expect("expose the key");
            let error = open_credential_key(&path, &census_sealed_under(&minted, 1))
                .expect_err("an exposed key must be refused");
            assert!(
                matches!(error, SecretError::KeyFilePermissions { .. }),
                "mode {exposed:o} must be refused for its mode, got {error}"
            );
            assert!(
                !error.to_string().contains(before.trim()),
                "the refusal must not print the key it just read"
            );
        }

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("restore");
        assert_eq!(
            open_credential_key(&path, &census_sealed_under(&minted, 1))
                .expect("an owner-only key still loads")
                .id(),
            minted.id()
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            before,
            "the refusals must not have rewritten the operator's key"
        );
    }

    /// The type alone does not keep cleartext out of a row: inside this crate a
    /// `SealedSecret` can hold a pre-encryption column. The durable write is
    /// where that gets refused.
    #[test]
    fn an_unwrapped_value_cannot_be_handed_to_a_durable_write() {
        let key = test_key(8);
        let sealed = key.seal_trakt(1, "token").expect("seal");
        assert_eq!(
            sealed.to_persist().expect("sealed persists"),
            sealed.as_stored()
        );

        for rejected in [
            "plain-bearer-token",
            "",
            "plxenc:v1:aabbccdd",
            "plxenc:v2:aabbccdd:0011",
        ] {
            let value = SealedSecret::from_stored(rejected);
            assert!(
                value.to_persist().is_err(),
                "`{rejected}` must not reach a durable row"
            );
        }
    }
}

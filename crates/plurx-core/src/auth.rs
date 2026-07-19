//! Password hashing and login tokens.
//!
//! Passwords are hashed with Argon2id (REQ-USER-1). Login tokens are random
//! 256-bit values; only their SHA-256 hash is persisted, so a database leak
//! never exposes a usable token (defense in depth alongside the plurx "no
//! cloud, no shared secret" posture).

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use sha2::{Digest, Sha256};

use crate::error::AuthError;

/// Hash a password for storage. Returns a PHC string (algorithm + params +
/// salt + hash) suitable for [`verify_password`].
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let mut salt_bytes = [0u8; 16];
    getrandom::getrandom(&mut salt_bytes).map_err(|e| AuthError::Rng(e.to_string()))?;
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| AuthError::Hash(e.to_string()))?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::Hash(e.to_string()))?;
    Ok(hash.to_string())
}

/// Verify a password against a stored PHC hash. Returns false on any parse or
/// mismatch — callers get a plain yes/no and cannot distinguish the reason.
pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Generate a fresh opaque login token (hex-encoded 256-bit random value).
/// Hand this to the client; store only [`hash_token`] of it.
pub fn generate_token() -> Result<String, AuthError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| AuthError::Rng(e.to_string()))?;
    Ok(hex::encode(bytes))
}

/// SHA-256 of a token, hex-encoded — the form stored in the database and
/// looked up on each request.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let hash = hash_password("hunter2").expect("hash");
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("hunter2", &hash));
        assert!(!verify_password("hunter3", &hash));
        assert!(!verify_password("hunter2", "not a real hash"));
    }

    #[test]
    fn salts_differ_between_hashes() {
        let a = hash_password("same").expect("a");
        let b = hash_password("same").expect("b");
        assert_ne!(a, b, "each hash must use a fresh salt");
    }

    #[test]
    fn tokens_are_unique_and_hash_stably() {
        let t1 = generate_token().expect("t1");
        let t2 = generate_token().expect("t2");
        assert_ne!(t1, t2);
        assert_eq!(t1.len(), 64); // 32 bytes hex
        assert_eq!(hash_token(&t1), hash_token(&t1));
        assert_ne!(hash_token(&t1), hash_token(&t2));
        // Known-answer: SHA-256("") — guards against accidental algo change.
        assert_eq!(
            hash_token(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}

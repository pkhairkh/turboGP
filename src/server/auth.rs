//! Authentication primitives for the pgwire server (Wave 65).
//!
//! Implements SCRAM-SHA-256 password storage and verification. The
//! [`PasswordManager`] stores username → salted password hash entries
//! (PBKDF2-HMAC-SHA-256, 4096 iterations, per RFC 7677). The pgwire
//! server uses it to verify the SCRAM client proof on each connection
//! when `ServerConfig::auth_required` is true.
//!
//! ## SCRAM-SHA-256 handshake (RFC 5802)
//!
//! 1. **Client → Server (SASLInitialResponse):** `n,,n=user,r=client_nonce`
//! 2. **Server → Client (AuthenticationSASLContinue):** `r=client_nonce+server_nonce,s=base64(salt),i=iterations`
//! 3. **Client → Server (SASLResponse):** `c=biws,r=combined_nonce,p=base64(client_proof)`
//! 4. **Server verifies client_proof**, then sends
//!    `v=base64(server_signature)` in a final AuthenticationSASLFinal
//!    message followed by AuthenticationOk.
//!
//! The salted password is derived once via PBKDF2-HMAC-SHA-256 at user
//! creation time and stored. Verification only needs the stored_key +
//! server_key (HMAC of the salted password), so the cleartext password
//! is never persisted.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Default PBKDF2 iteration count (RFC 7677 recommends 4096 for SCRAM-SHA-256).
pub const DEFAULT_ITERATIONS: u32 = 4096;

/// A stored credential: the salt, iteration count, and the two derived
/// keys (stored_key = SHA-256(client_key), server_key = HMAC-SHA-256(salted_password, "Server Key")).
///
/// We store stored_key and server_key directly (not the salted_password)
/// so that even a full database leak cannot be used to impersonate the
/// client (the salted_password would be needed to derive a new client_key).
#[derive(Debug, Clone)]
pub struct StoredCredential {
    /// Random per-user salt (raw bytes).
    pub salt: Vec<u8>,
    /// PBKDF2 iteration count.
    pub iterations: u32,
    /// SHA-256(client_key) where client_key = HMAC-SHA-256(salted_password, "Client Key").
    pub stored_key: [u8; 32],
    /// HMAC-SHA-256(salted_password, "Server Key").
    pub server_key: [u8; 32],
}

impl StoredCredential {
    /// Derive a credential from a cleartext password and salt.
    ///
    /// - salted_password = PBKDF2-HMAC-SHA-256(password, salt, iterations)
    /// - client_key      = HMAC-SHA-256(salted_password, "Client Key")
    /// - stored_key      = SHA-256(client_key)
    /// - server_key      = HMAC-SHA-256(salted_password, "Server Key")
    pub fn from_password(password: &str, salt: &[u8], iterations: u32) -> Self {
        let mut salted = [0u8; 32];
        pbkdf2::<HmacSha256>(password.as_bytes(), salt, iterations, &mut salted);
        let mut client_key = [0u8; 32];
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&salted).expect("hmac key len");
        mac.update(b"Client Key");
        client_key.copy_from_slice(&mac.finalize().into_bytes());
        let mut stored_key = [0u8; 32];
        let mut h = Sha256::new();
        h.update(client_key);
        stored_key.copy_from_slice(&h.finalize());
        let mut server_key = [0u8; 32];
        let mut mac2 = <HmacSha256 as Mac>::new_from_slice(&salted).expect("hmac key len");
        mac2.update(b"Server Key");
        server_key.copy_from_slice(&mac2.finalize().into_bytes());
        StoredCredential { salt: salt.to_vec(), iterations, stored_key, server_key }
    }
}

/// In-memory user → credential registry. Shared via `Arc<RwLock<PasswordManager>>`
/// between the server (which reads on each connection) and the DDL
/// intercept in `PgConn` (which writes on CREATE USER / DROP USER).
#[derive(Debug, Clone, Default)]
pub struct PasswordManager {
    users: std::collections::HashMap<String, StoredCredential>,
}

impl PasswordManager {
    /// Create an empty password manager.
    pub fn new() -> Self {
        Self { users: std::collections::HashMap::new() }
    }

    /// Create (or replace) a user with the given cleartext password.
    /// A fresh random salt is generated so re-creating a user with the
    /// same password yields a different stored_key.
    pub fn create_user(&mut self, username: &str, password: &str) {
        self.create_user_with_salt(username, password, &random_salt(), DEFAULT_ITERATIONS)
    }

    /// Create (or replace) a user with a specific salt + iteration count.
    /// Used by tests to make the handshake deterministic.
    pub fn create_user_with_salt(
        &mut self,
        username: &str,
        password: &str,
        salt: &[u8],
        iterations: u32,
    ) {
        let cred = StoredCredential::from_password(password, salt, iterations);
        self.users.insert(username.to_string(), cred);
    }

    /// Drop a user. Returns true if the user existed.
    pub fn drop_user(&mut self, username: &str) -> bool {
        self.users.remove(username).is_some()
    }

    /// Look up a user's stored credential.
    pub fn get(&self, username: &str) -> Option<&StoredCredential> {
        self.users.get(username)
    }

    /// True if a user exists.
    pub fn exists(&self, username: &str) -> bool {
        self.users.contains_key(username)
    }

    /// Number of registered users.
    pub fn len(&self) -> usize {
        self.users.len()
    }
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }
}

/// Generate a 16-byte random salt using the OS RNG via `rand`.
fn random_salt() -> Vec<u8> {
    use rand::RngCore;
    let mut buf = [0u8; 16];
    rand::rng().fill_bytes(&mut buf);
    buf.to_vec()
}

/// Outcome of verifying a SCRAM-SHA-256 client final message.
#[derive(Debug)]
pub enum ScramOutcome {
    /// The proof was valid. Carries the `v=` base64 server signature
    /// that the server should send in AuthenticationSASLFinal.
    Ok { server_signature_b64: String },
    /// The proof was invalid (wrong password, malformed message, etc.).
    Invalid,
}

/// Verify a SCRAM-SHA-256 client-final message against the stored
/// credential.
///
/// Inputs:
/// - `cred`: the user's stored credential (salt, iterations, stored_key, server_key).
/// - `client_first_bare`: the part of the client-first message after the
///   gs2-header, e.g. `n=alice,r=clientnonce` (no leading `n,,`).
/// - `server_first`: the full server-first message we sent, e.g.
///   `r=combinednonce,s=base64salt,i=4096`.
/// - `client_final`: the client's final message, e.g.
///   `c=biws,r=combinednonce,p=base64proof`.
///
/// Algorithm (RFC 5802):
/// 1. Parse the client_final to extract `r=` (combined nonce) and `p=` (proof).
/// 2. Reconstruct `AuthMessage = client_first_bare + "," + server_first + "," + client_final_without_proof`.
/// 3. `client_signature = HMAC-SHA-256(stored_key, AuthMessage)`.
/// 4. `client_key = client_proof XOR client_signature`.
/// 5. Verify `SHA-256(client_key) == stored_key`. If not, return Invalid.
/// 6. `server_signature = HMAC-SHA-256(server_key, AuthMessage)`. Return Ok with base64.
pub fn verify_scram(
    cred: &StoredCredential,
    client_first_bare: &str,
    server_first: &str,
    client_final: &str,
) -> ScramOutcome {
    // Parse client_final: c=biws,r=combinednonce,p=base64proof
    let mut channel_binding = None;
    let mut nonce = None;
    let mut proof_b64 = None;
    for part in client_final.split(',') {
        if let Some(rest) = part.strip_prefix("c=") {
            channel_binding = Some(rest);
        } else if let Some(rest) = part.strip_prefix("r=") {
            nonce = Some(rest);
        } else if let Some(rest) = part.strip_prefix("p=") {
            proof_b64 = Some(rest);
        }
    }
    let _ = channel_binding;
    let _ = nonce;
    let proof_b64 = match proof_b64 {
        Some(p) => p,
        None => return ScramOutcome::Invalid,
    };
    let client_proof = match B64.decode(proof_b64.as_bytes()) {
        Ok(b) => b,
        Err(_) => return ScramOutcome::Invalid,
    };
    if client_proof.len() != 32 {
        return ScramOutcome::Invalid;
    }
    // Strip the p=... trailing part to form client_final_without_proof.
    // client_final_without_proof = everything up to (but not including) the
    // comma before "p=".
    let without_proof = match client_final.rfind(",p=") {
        Some(idx) => &client_final[..idx],
        None => return ScramOutcome::Invalid,
    };
    let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
    // client_signature = HMAC-SHA-256(stored_key, AuthMessage)
    let mut mac = match <HmacSha256 as Mac>::new_from_slice(&cred.stored_key) {
        Ok(m) => m,
        Err(_) => return ScramOutcome::Invalid,
    };
    mac.update(auth_message.as_bytes());
    let client_sig = mac.finalize().into_bytes();
    // client_key = client_proof XOR client_signature
    let mut client_key = [0u8; 32];
    for i in 0..32 {
        client_key[i] = client_proof[i] ^ client_sig[i];
    }
    // Verify SHA-256(client_key) == stored_key
    let mut h = Sha256::new();
    h.update(client_key);
    let computed_stored: [u8; 32] = h.finalize().into();
    if computed_stored != cred.stored_key {
        return ScramOutcome::Invalid;
    }
    // server_signature = HMAC-SHA-256(server_key, AuthMessage)
    let mut mac2 = match <HmacSha256 as Mac>::new_from_slice(&cred.server_key) {
        Ok(m) => m,
        Err(_) => return ScramOutcome::Invalid,
    };
    mac2.update(auth_message.as_bytes());
    let server_sig = mac2.finalize().into_bytes();
    let server_signature_b64 = B64.encode(server_sig);
    ScramOutcome::Ok { server_signature_b64 }
}

/// Configuration for TLS upgrade on the pgwire listener (Wave 65).
///
/// When `ServerConfig::tls` is `Some`, the server should respond 'S' to
/// an SSLRequest and upgrade the TCP stream to TLS before reading the
/// startup message. The actual TLS upgrade requires `tokio-rustls`; the
/// struct is defined here so callers can configure it, but the current
/// implementation falls back to plaintext (responds 'N') until the TLS
/// listener is wired in a future wave.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Path to the PEM-encoded certificate chain.
    pub cert_path: String,
    /// Path to the PEM-encoded private key matching the certificate.
    pub key_path: String,
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_lookup_user() {
        let mut mgr = PasswordManager::new();
        mgr.create_user_with_salt("alice", "secret", &[1, 2, 3, 4], 4096);
        assert!(mgr.exists("alice"));
        assert!(!mgr.exists("bob"));
        let cred = mgr.get("alice").unwrap();
        assert_eq!(cred.iterations, 4096);
        assert_eq!(cred.salt, vec![1, 2, 3, 4]);
    }

    #[test]
    fn drop_user() {
        let mut mgr = PasswordManager::new();
        mgr.create_user_with_salt("alice", "secret", &[1, 2, 3, 4], 4096);
        assert!(mgr.drop_user("alice"));
        assert!(!mgr.exists("alice"));
        assert!(!mgr.drop_user("alice"));
    }

    /// End-to-end SCRAM-SHA-256 verification with a known-correct
    /// password. Re-derives the client proof using the algorithm from
    /// RFC 5802 and verifies our `verify_scram` accepts it.
    #[test]
    fn scram_handshake_correct_password() {
        let salt = vec![0xAA; 16];
        let iterations = 4096u32;
        let password = "correct horse battery staple";

        // Derive the credential that the server would store.
        let cred = StoredCredential::from_password(password, &salt, iterations);

        // Simulate the handshake messages.
        let client_nonce = "fyko+d2lbbFgONRv9qkxdawL";
        let server_nonce = "3rfcNHYJY1ZVvWVs7jJnoNew";
        let combined_nonce = format!("{client_nonce}{server_nonce}");
        let client_first_bare = format!("n=alice,r={client_nonce}");
        let server_first = format!("r={combined_nonce},s={},i={iterations}", B64.encode(&salt));
        // Compute client proof.
        let mut salted = [0u8; 32];
        pbkdf2::<HmacSha256>(password.as_bytes(), &salt, iterations, &mut salted);
        let mut cmac = <HmacSha256 as Mac>::new_from_slice(&salted).unwrap();
        cmac.update(b"Client Key");
        let client_key: [u8; 32] = cmac.finalize().into_bytes().into();
        let client_final_without_proof = format!("c=biws,r={combined_nonce}");
        let auth_message =
            format!("{client_first_bare},{server_first},{client_final_without_proof}");
        let mut smac = <HmacSha256 as Mac>::new_from_slice(&cred.stored_key).unwrap();
        smac.update(auth_message.as_bytes());
        let client_sig: [u8; 32] = smac.finalize().into_bytes().into();
        let mut client_proof = [0u8; 32];
        for i in 0..32 {
            client_proof[i] = client_key[i] ^ client_sig[i];
        }
        let client_final = format!("{client_final_without_proof},p={}", B64.encode(client_proof));

        match verify_scram(&cred, &client_first_bare, &server_first, &client_final) {
            ScramOutcome::Ok { server_signature_b64 } => {
                // Verify the server signature matches what the client would compute.
                let mut vsmac = <HmacSha256 as Mac>::new_from_slice(&salted).unwrap();
                vsmac.update(b"Server Key");
                let server_key: [u8; 32] = vsmac.finalize().into_bytes().into();
                let mut ssig = <HmacSha256 as Mac>::new_from_slice(&server_key).unwrap();
                ssig.update(auth_message.as_bytes());
                let expected: [u8; 32] = ssig.finalize().into_bytes().into();
                assert_eq!(server_signature_b64, B64.encode(expected));
            }
            ScramOutcome::Invalid => panic!("correct password must verify"),
        }
    }

    /// A wrong password must produce an invalid proof.
    #[test]
    fn scram_handshake_wrong_password() {
        let salt = vec![0xBB; 16];
        let iterations = 4096u32;
        let cred = StoredCredential::from_password("right-password", &salt, iterations);

        let client_nonce = "abc123";
        let server_nonce = "def456";
        let combined_nonce = format!("{client_nonce}{server_nonce}");
        let client_first_bare = format!("n=alice,r={client_nonce}");
        let server_first = format!("r={combined_nonce},s={},i={iterations}", B64.encode(&salt));

        // Derive client proof using the WRONG password.
        let wrong_password = "wrong-password";
        let mut salted = [0u8; 32];
        pbkdf2::<HmacSha256>(wrong_password.as_bytes(), &salt, iterations, &mut salted);
        let mut cmac = <HmacSha256 as Mac>::new_from_slice(&salted).unwrap();
        cmac.update(b"Client Key");
        let client_key: [u8; 32] = cmac.finalize().into_bytes().into();
        let client_final_without_proof = format!("c=biws,r={combined_nonce}");
        let auth_message =
            format!("{client_first_bare},{server_first},{client_final_without_proof}");
        let mut smac = <HmacSha256 as Mac>::new_from_slice(&cred.stored_key).unwrap();
        smac.update(auth_message.as_bytes());
        let client_sig: [u8; 32] = smac.finalize().into_bytes().into();
        let mut client_proof = [0u8; 32];
        for i in 0..32 {
            client_proof[i] = client_key[i] ^ client_sig[i];
        }
        let client_final = format!("{client_final_without_proof},p={}", B64.encode(client_proof));

        match verify_scram(&cred, &client_first_bare, &server_first, &client_final) {
            ScramOutcome::Ok { .. } => panic!("wrong password must NOT verify"),
            ScramOutcome::Invalid => {}
        }
    }
}

//! Optional signed sender identity (Tier-2 phase 2d), behind the `sign` feature.
//!
//! Ed25519 signatures make a cross-store `from` **unforgeable**: a sender signs a
//! canonical encoding of its intent with a private key, and a receiver verifies the
//! signature against the sender's registered public key BEFORE committing the
//! pulled message into its own inbox. Without this feature the whole module is
//! compiled out and weave delivers exactly as Tier-2 phases 2a–2c do, on the
//! advisory allowlist + origin-attribution model.
//!
//! Layering: this module is a LOW layer — it depends only on `config` (for the key
//! directory) and std. `model`/`store` never depend up on it; the canonical-bytes
//! helper is pure and the signing/verification calls are consumed downward by
//! `store`'s pull driver (verify) and `main`/`mcp` (sign on enqueue, key CLI).
//!
//! Key material: the PRIVATE key lives in a file under the config dir
//! (`~/.config/weave/ed25519.key`), written 0600 (owner-only) and NEVER logged,
//! printed, or placed in the DB. Only the 32-byte PUBLIC key is registered (in the
//! `keys` table) and printed. The canonical message format is stable and
//! unambiguous (length-prefixed fields) so a signature made by one build verifies
//! against any other build.

#![cfg(feature = "sign")]

use anyhow::{bail, Context, Result};
use ed25519_dalek::{
    Signature, Signer, SigningKey, Verifier, VerifyingKey, SECRET_KEY_LENGTH, SIGNATURE_LENGTH,
};
use std::path::PathBuf;

/// Hard cap on an encoded public key / signature string accepted from any source
/// before it is stored or parsed. A 32-byte key is 64 hex chars and a 64-byte
/// signature is 128 hex chars; this generous bound rejects an unbounded or hostile
/// value at the store/verify seam (defense in depth, mirroring the ident/body caps)
/// while leaving slack for any reasonable encoding.
pub const MAX_KEY_HEX_LEN: usize = 256;

/// Absolute path to this session's private signing-key file
/// (`<config-dir>/ed25519.key`). Sits next to `config.toml` so the same 0700 dir
/// hardening applies. The file holds the 32-byte secret key as lowercase hex.
pub fn key_path() -> PathBuf {
    crate::config::config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ed25519.key")
}

/// Encode bytes as lowercase hex (no separators). Used for the stored public key,
/// the on-disk private key, and the intent signature. Crate-free.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decode a lowercase/uppercase hex string into bytes. Rejects odd length and any
/// non-hex digit so a malformed key/signature surfaces as an error rather than a
/// silent misverify.
pub fn from_hex(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        bail!("hex string has odd length");
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_val(bytes[i])?;
        let lo = hex_val(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_val(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => bail!("invalid hex digit"),
    }
}

/// The canonical byte encoding signed/verified for an intent: the tuple
/// `(from, to, body)` serialized **length-prefixed** so no field boundary can be
/// ambiguous. Each field is emitted as its 8-byte big-endian byte length followed
/// by its UTF-8 bytes. A domain-separation prefix (`"weave-intent-v1\0"`) pins the
/// message to this purpose and version so a signature can never be replayed in
/// another context.
///
/// The sender's wall-clock `created`/`ts` is deliberately NOT signed: it is purely
/// advisory (the RECEIVER re-stamps every committed message with its own `now()`,
/// anchoring ordering locally), so binding it would add no integrity value while
/// forcing the signer and the store's self-stamped `ts` to agree exactly. The
/// security-relevant fields — who it is from, who it is to, and the body — ARE
/// bound, which is what makes a cross-store `from` unforgeable. This is the single
/// source of truth for both signing and verifying: pure, deterministic, stable
/// across builds/backends.
pub fn canonical_message(from: &str, to: &str, body: &str) -> Vec<u8> {
    const DOMAIN: &[u8] = b"weave-intent-v1\0";
    let mut out = Vec::with_capacity(DOMAIN.len() + from.len() + to.len() + body.len() + 32);
    out.extend_from_slice(DOMAIN);
    for field in [from.as_bytes(), to.as_bytes(), body.as_bytes()] {
        out.extend_from_slice(&(field.len() as u64).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// Generate a fresh Ed25519 keypair, persist the PRIVATE key 0600 under the config
/// dir (creating the dir 0700 if needed), and return the hex-encoded PUBLIC key.
/// Refuses to clobber an existing key file (a keypair is long-lived identity; an
/// accidental overwrite would silently invalidate every already-registered pubkey).
/// The private key is never logged, printed, or returned — only the public key is.
pub fn generate_keypair() -> Result<String> {
    let path = key_path();
    if path.exists() {
        bail!(
            "a signing key already exists at {} (refusing to overwrite); \
             delete it manually to rotate",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating config dir for signing key")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let signing = new_signing_key()?;
    let secret_hex = to_hex(signing.to_bytes().as_slice());
    write_secret(&path, &secret_hex)?;
    Ok(to_hex(signing.verifying_key().as_bytes()))
}

/// Construct a fresh signing key from 32 OS-CSPRNG bytes. We fill the secret
/// scalar directly via `getrandom` (the OS entropy source) and build the key with
/// `SigningKey::from_bytes`, avoiding any coupling to dalek's `rand_core` version.
/// The raw secret bytes never leave this function except as the persisted 0600 file.
fn new_signing_key() -> Result<SigningKey> {
    let mut secret = [0u8; SECRET_KEY_LENGTH];
    getrandom::getrandom(&mut secret)
        .map_err(|e| anyhow::anyhow!("reading OS entropy for key generation: {e}"))?;
    Ok(SigningKey::from_bytes(&secret))
}

/// Write the hex secret to `path` with 0600 perms, atomically refusing to clobber
/// an existing file (`create_new`). The secret is the only thing here that must
/// never leak: it is written straight to the owner-only file and never returned.
fn write_secret(path: &PathBuf, secret_hex: &str) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).context("creating signing key file")?;
    f.write_all(secret_hex.as_bytes())
        .context("writing signing key")?;
    Ok(())
}

/// Load this session's signing key from [`key_path`], if present. `Ok(None)` when
/// no key file exists (this session simply does not sign — fall back to advisory
/// identity). Errors only on a present-but-corrupt key file.
pub fn load_signing_key() -> Result<Option<SigningKey>> {
    let path = key_path();
    let hex = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading signing key file"),
    };
    let bytes = from_hex(hex.trim()).context("decoding signing key file")?;
    if bytes.len() != SECRET_KEY_LENGTH {
        bail!("signing key file is not a {SECRET_KEY_LENGTH}-byte ed25519 secret key");
    }
    let mut arr = [0u8; SECRET_KEY_LENGTH];
    arr.copy_from_slice(&bytes);
    Ok(Some(SigningKey::from_bytes(&arr)))
}

/// This session's hex-encoded PUBLIC key, if a signing key is configured.
pub fn local_public_key() -> Result<Option<String>> {
    Ok(load_signing_key()?.map(|sk| to_hex(sk.verifying_key().as_bytes())))
}

/// Sign the canonical message for an intent with `key`, returning the hex-encoded
/// 64-byte signature to store in `outbox.sig`. Deterministic (Ed25519), so the same
/// intent always yields the same signature.
pub fn sign_intent(key: &SigningKey, from: &str, to: &str, body: &str) -> String {
    let msg = canonical_message(from, to, body);
    let sig: Signature = key.sign(&msg);
    to_hex(&sig.to_bytes())
}

/// Verify `sig_hex` over the canonical `(from,to,body)` message against the
/// hex-encoded public key `pubkey_hex`. Returns `Ok(true)` only on a valid
/// signature; a malformed key/signature or a verification failure returns
/// `Ok(false)` (never an `Err`) so the caller can treat "unverifiable" uniformly
/// with "unsigned" under the fallback policy. A tampered/forged signature can never
/// return `true`.
pub fn verify_intent(
    pubkey_hex: &str,
    sig_hex: &str,
    from: &str,
    to: &str,
    body: &str,
) -> Result<bool> {
    if pubkey_hex.len() > MAX_KEY_HEX_LEN || sig_hex.len() > MAX_KEY_HEX_LEN {
        return Ok(false);
    }
    let pk_bytes = match from_hex(pubkey_hex) {
        Ok(b) if b.len() == ed25519_dalek::PUBLIC_KEY_LENGTH => b,
        _ => return Ok(false),
    };
    let sig_bytes = match from_hex(sig_hex) {
        Ok(b) if b.len() == SIGNATURE_LENGTH => b,
        _ => return Ok(false),
    };
    let mut pk_arr = [0u8; ed25519_dalek::PUBLIC_KEY_LENGTH];
    pk_arr.copy_from_slice(&pk_bytes);
    let verifying = match VerifyingKey::from_bytes(&pk_arr) {
        Ok(k) => k,
        Err(_) => return Ok(false),
    };
    let mut sig_arr = [0u8; SIGNATURE_LENGTH];
    sig_arr.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(&sig_arr);
    let msg = canonical_message(from, to, body);
    Ok(verifying.verify(&msg, &signature).is_ok())
}

/// Validate a hex public key supplied by a user (`weave key add`) before it is
/// stored: bounded length, valid hex, and exactly a 32-byte ed25519 key. Keeps a
/// malformed/oversized value out of the `keys` table at the registration seam.
pub fn check_pubkey(pubkey_hex: &str) -> Result<()> {
    if pubkey_hex.len() > MAX_KEY_HEX_LEN {
        bail!(
            "public key is too long ({} chars; max {MAX_KEY_HEX_LEN})",
            pubkey_hex.len()
        );
    }
    let bytes = from_hex(pubkey_hex).context("public key is not valid hex")?;
    if bytes.len() != ed25519_dalek::PUBLIC_KEY_LENGTH {
        bail!(
            "public key must be a {}-byte ed25519 key ({} hex chars)",
            ed25519_dalek::PUBLIC_KEY_LENGTH,
            ed25519_dalek::PUBLIC_KEY_LENGTH * 2
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sign→verify round-trips, and ANY single-field mutation fails verification —
    /// the core unforgeability property.
    /// A deterministic test key from a fixed seed (no RNG needed) so the tests are
    /// reproducible and need no `rand` dependency.
    fn test_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; SECRET_KEY_LENGTH])
    }

    #[test]
    fn sign_then_verify_roundtrips_and_detects_tamper() {
        let key = test_key(7);
        let pk = to_hex(key.verifying_key().as_bytes());
        let sig = sign_intent(&key, "alice", "bob", "hello");

        assert!(verify_intent(&pk, &sig, "alice", "bob", "hello").unwrap());

        // Each mutated field breaks verification.
        assert!(!verify_intent(&pk, &sig, "mallory", "bob", "hello").unwrap());
        assert!(!verify_intent(&pk, &sig, "alice", "carol", "hello").unwrap());
        assert!(!verify_intent(&pk, &sig, "alice", "bob", "HELLO").unwrap());
    }

    /// A signature from a DIFFERENT key never verifies (the spoofed-`from` case).
    #[test]
    fn wrong_key_never_verifies() {
        let real = test_key(1);
        let attacker = test_key(2);
        let real_pk = to_hex(real.verifying_key().as_bytes());
        // Attacker signs claiming to be from "alice".
        let forged = sign_intent(&attacker, "alice", "bob", "x");
        assert!(
            !verify_intent(&real_pk, &forged, "alice", "bob", "x").unwrap(),
            "a signature by the wrong key must never verify"
        );
    }

    /// Malformed / unsigned inputs verify to false, never erroring, so the caller
    /// can treat them uniformly with "unsigned".
    #[test]
    fn malformed_inputs_are_false_not_error() {
        let pk = to_hex(&[0u8; 32]);
        assert!(!verify_intent(&pk, "", "a", "b", "c").unwrap());
        assert!(!verify_intent("", "deadbeef", "a", "b", "c").unwrap());
        assert!(!verify_intent("zz", "zz", "a", "b", "c").unwrap());
        assert!(!verify_intent(&"f".repeat(MAX_KEY_HEX_LEN + 2), "00", "a", "b", "c").unwrap());
    }

    /// The canonical encoding is unambiguous: shifting a delimiter between fields
    /// produces different bytes (length-prefixing prevents `("ab","c")` colliding
    /// with `("a","bc")`).
    #[test]
    fn canonical_message_is_unambiguous() {
        assert_ne!(
            canonical_message("ab", "c", "x"),
            canonical_message("a", "bc", "x")
        );
        // Stable: same inputs ⇒ same bytes.
        assert_eq!(
            canonical_message("a", "b", "body"),
            canonical_message("a", "b", "body")
        );
    }

    #[test]
    fn hex_roundtrips_and_rejects_bad() {
        let bytes = [0x00u8, 0xff, 0x10, 0xab];
        assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
        assert!(from_hex("abc").is_err(), "odd length rejected");
        assert!(from_hex("zz").is_err(), "non-hex rejected");
    }

    #[test]
    fn check_pubkey_bounds_and_validates() {
        let key = test_key(9);
        let pk = to_hex(key.verifying_key().as_bytes());
        assert!(check_pubkey(&pk).is_ok());
        assert!(check_pubkey("deadbeef").is_err(), "too short");
        assert!(
            check_pubkey(&"f".repeat(MAX_KEY_HEX_LEN + 2)).is_err(),
            "too long"
        );
        assert!(check_pubkey("zz").is_err(), "non-hex");
    }

    use proptest::prelude::*;

    proptest! {
        /// For ANY `(from, to, body)`, a signature over them verifies; and ANY
        /// single-field mutation breaks verification (the new-invariant property).
        #[test]
        fn signature_verifies_and_any_mutation_fails(
            from in ".{0,64}",
            to in ".{0,64}",
            body in ".{0,256}",
            seed in any::<u8>(),
        ) {
            let key = test_key(seed);
            let pk = to_hex(key.verifying_key().as_bytes());
            let sig = sign_intent(&key, &from, &to, &body);
            prop_assert!(verify_intent(&pk, &sig, &from, &to, &body).unwrap());

            // Mutating any single field (to a guaranteed-different value) fails.
            let mut_from = format!("{from}~");
            let mut_to = format!("{to}~");
            let mut_body = format!("{body}~");
            prop_assert!(!verify_intent(&pk, &sig, &mut_from, &to, &body).unwrap());
            prop_assert!(!verify_intent(&pk, &sig, &from, &mut_to, &body).unwrap());
            prop_assert!(!verify_intent(&pk, &sig, &from, &to, &mut_body).unwrap());
        }
    }
}

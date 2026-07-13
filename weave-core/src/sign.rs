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

/// Semantic fields covered by the current (`v2`) cross-store intent signature.
/// `trace_id` is deliberately excluded because it is attempt-local diagnostics:
/// exact retries preserve the first stored trace while allowing a later attempt
/// to carry a new one. Every field that changes delivery or message meaning is
/// included, with `None` distinct from an explicitly empty value.
#[derive(Debug, Clone, Copy)]
pub struct IntentSignatureFields<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub to_host: &'a str,
    pub subject: Option<&'a str>,
    pub body: &'a str,
    pub idempotency_key: Option<&'a str>,
    pub priority: &'a str,
    pub ttl: i64,
}

/// Canonical byte encoding for a complete intent's semantic tuple. Callers pass
/// the Store-canonical host and priority values. Length prefixes and explicit
/// option markers make every field boundary and `None`/`Some` shape unambiguous.
pub fn canonical_intent_v2(fields: &IntentSignatureFields<'_>) -> Vec<u8> {
    const DOMAIN: &[u8] = b"weave-intent-v2\0";

    fn push_field(out: &mut Vec<u8>, field: &[u8]) {
        out.extend_from_slice(&(field.len() as u64).to_be_bytes());
        out.extend_from_slice(field);
    }

    fn push_optional(out: &mut Vec<u8>, field: Option<&str>) {
        match field {
            Some(field) => {
                out.push(1);
                push_field(out, field.as_bytes());
            }
            None => out.push(0),
        }
    }

    let mut out = Vec::with_capacity(
        DOMAIN.len()
            + fields.from.len()
            + fields.to.len()
            + fields.to_host.len()
            + fields.subject.map_or(0, str::len)
            + fields.body.len()
            + fields.idempotency_key.map_or(0, str::len)
            + 80,
    );
    out.extend_from_slice(DOMAIN);
    push_field(&mut out, fields.from.as_bytes());
    push_field(&mut out, fields.to.as_bytes());
    push_field(&mut out, fields.to_host.as_bytes());
    push_optional(&mut out, fields.subject);
    push_field(&mut out, fields.body.as_bytes());
    push_optional(&mut out, fields.idempotency_key);
    push_field(&mut out, fields.priority.as_bytes());
    out.extend_from_slice(&fields.ttl.to_be_bytes());
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

/// Rotate this session's signing key (R6, config-based overlap). If a key file
/// exists, ARCHIVE it (rename to a non-clobbering `ed25519.key.<unix_ts>.bak`,
/// 0600) instead of refusing, then generate a fresh keypair and write it. Returns
/// `(old_pubkey_hex, new_pubkey_hex)` — `old` is `None` when there was no prior key
/// (rotation reduces to a plain generate). The OLD PRIVATE key is moved, never
/// printed; only PUBLIC keys/fingerprints are returned. This is the ONLY path that
/// may displace an existing key file; plain `generate_keypair` still refuses to
/// clobber. During overlap the receiver should trust BOTH fingerprints in
/// `WEAVE_TRUST` and keep the OLD pubkey registered (`weave key add`).
pub fn rotate_keypair() -> Result<(Option<String>, String)> {
    let path = key_path();
    let old_pub = if path.exists() {
        // Recover the OLD public key BEFORE moving the file, so the caller can
        // print/keep it registered during the overlap window. A corrupt old key is
        // non-fatal here: archive it anyway and report no old pubkey.
        let old = load_signing_key()
            .ok()
            .flatten()
            .map(|sk| to_hex(sk.verifying_key().as_bytes()));
        let backup = archive_path(&path);
        std::fs::rename(&path, &backup).with_context(|| {
            format!(
                "archiving existing signing key {} -> {}",
                path.display(),
                backup.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600));
        }
        old
    } else {
        None
    };
    let new_pub = generate_keypair()?;
    Ok((old_pub, new_pub))
}

/// A non-clobbering archive path for the current key file:
/// `<key_path>.<unix_ts>.bak`, bumping a counter suffix on the (rare) collision so
/// two rotations in the same second never overwrite each other.
fn archive_path(path: &std::path::Path) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let base = format!("{}.{ts}.bak", path.display());
    let mut candidate = PathBuf::from(&base);
    let mut n = 1u32;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{base}.{n}"));
        n += 1;
    }
    candidate
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

/// Sign the complete v2 intent tuple. The `v2:` marker is stored alongside the
/// hex signature so receivers can continue accepting already-queued v1 rows
/// without guessing which canonical encoding was used.
pub fn sign_intent_v2(key: &SigningKey, fields: &IntentSignatureFields<'_>) -> String {
    let msg = canonical_intent_v2(fields);
    let sig: Signature = key.sign(&msg);
    format!("v2:{}", to_hex(&sig.to_bytes()))
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

/// Verify a `v2:` signature over the complete semantic intent tuple. A missing or
/// unknown marker, malformed key/signature, or failed verification is simply
/// `Ok(false)`, matching the legacy verifier's fail-closed caller contract.
pub fn verify_intent_v2(
    pubkey_hex: &str,
    encoded_sig: &str,
    fields: &IntentSignatureFields<'_>,
) -> Result<bool> {
    let Some(sig_hex) = encoded_sig.strip_prefix("v2:") else {
        return Ok(false);
    };
    if pubkey_hex.len() > MAX_KEY_HEX_LEN || encoded_sig.len() > MAX_KEY_HEX_LEN {
        return Ok(false);
    }
    let pk_bytes = match from_hex(pubkey_hex) {
        Ok(bytes) if bytes.len() == ed25519_dalek::PUBLIC_KEY_LENGTH => bytes,
        _ => return Ok(false),
    };
    let sig_bytes = match from_hex(sig_hex) {
        Ok(bytes) if bytes.len() == SIGNATURE_LENGTH => bytes,
        _ => return Ok(false),
    };
    let mut pk = [0u8; ed25519_dalek::PUBLIC_KEY_LENGTH];
    pk.copy_from_slice(&pk_bytes);
    let verifying = match VerifyingKey::from_bytes(&pk) {
        Ok(key) => key,
        Err(_) => return Ok(false),
    };
    let mut sig = [0u8; SIGNATURE_LENGTH];
    sig.copy_from_slice(&sig_bytes);
    let msg = canonical_intent_v2(fields);
    Ok(verifying.verify(&msg, &Signature::from_bytes(&sig)).is_ok())
}

/// Number of hex chars of the SHA-256 digest shown in the DISPLAY fingerprint
/// (`SHA256:` + this many chars). 16 hex = 8 bytes = 64 bits — short and stable
/// for a local mesh. This is a DISPLAY/UX convenience ONLY: trust decisions are
/// NEVER made on the truncated form (see [`fingerprint_full`]); verification is
/// always over the full pubkey + signature, never the fingerprint.
pub const FINGERPRINT_DISPLAY_HEX: usize = 16;

/// The FULL SHA-256 digest of a hex public key, rendered as 64 lowercase hex
/// chars, WITHOUT the `SHA256:` label. This is the canonical value a trust/revoked
/// entry is matched against (R3: never truncate a trust decision). Returns `None`
/// for a malformed/oversized (`> MAX_KEY_HEX_LEN`) or non-32-byte pubkey; never
/// panics, and NEVER takes or hashes the secret key (the input is the PUBLIC key).
pub fn fingerprint_full(pubkey_hex: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let trimmed = pubkey_hex.trim();
    if trimmed.len() > MAX_KEY_HEX_LEN {
        return None;
    }
    let bytes = from_hex(trimmed).ok()?;
    if bytes.len() != ed25519_dalek::PUBLIC_KEY_LENGTH {
        return None;
    }
    let digest = Sha256::digest(&bytes);
    Some(to_hex(&digest))
}

/// The DISPLAY fingerprint of a hex public key: `"SHA256:"` + the first
/// [`FINGERPRINT_DISPLAY_HEX`] hex chars of the full SHA-256 digest. Short, stable,
/// and derived ONLY from the PUBLIC key. Returns `None` for a malformed/oversized
/// pubkey; never panics. DISPLAY ONLY — never the basis of a trust decision (use
/// [`fingerprint_full`] / [`fingerprint_matches`] for that).
pub fn fingerprint(pubkey_hex: &str) -> Option<String> {
    let full = fingerprint_full(pubkey_hex)?;
    let short = &full[..FINGERPRINT_DISPLAY_HEX.min(full.len())];
    Some(format!("SHA256:{short}"))
}

/// Does a trust/revoked-list `entry` designate the pubkey `pubkey_hex`? Two accepted
/// forms, BOTH compared against the FULL SHA-256 digest or the full pubkey hex (R3 —
/// never the truncated display form):
///
/// 1. a `SHA256:<full-64-hex>` fingerprint (full digest, case-insensitive hex),
/// 2. a bare full pubkey hex (64 chars) — its own digest is compared.
///
/// A truncated `SHA256:<16-hex>` display string does NOT match (deliberately: a
/// trust decision must use the full digest, never the display prefix). Returns
/// `false` on any malformed input; never panics. The caller passes the receiver's
/// registered pubkey for `from`, so an entry only matches a sender weave actually
/// has a key for (R5).
pub fn fingerprint_matches(entry: &str, pubkey_hex: &str) -> bool {
    let full = match fingerprint_full(pubkey_hex) {
        Some(f) => f,
        None => return false,
    };
    let entry = entry.trim();
    // Form 1: `SHA256:<hex>` — compare the full digest, case-insensitively.
    if let Some(rest) = entry.strip_prefix("SHA256:") {
        return rest.eq_ignore_ascii_case(&full);
    }
    // Form 2: a bare full pubkey hex — derive its digest and compare.
    if let Some(other) = fingerprint_full(entry) {
        return other == full;
    }
    false
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

    #[test]
    fn v2_signature_binds_every_delivery_semantic() {
        let key = test_key(17);
        let pk = to_hex(key.verifying_key().as_bytes());
        let fields = IntentSignatureFields {
            from: "alice",
            to: "bob",
            to_host: "host-b",
            subject: Some("topic"),
            body: "hello",
            idempotency_key: Some("event_1"),
            priority: "urgent",
            ttl: 600,
        };
        let sig = sign_intent_v2(&key, &fields);
        assert!(sig.starts_with("v2:"));
        assert!(verify_intent_v2(&pk, &sig, &fields).unwrap());

        let mutations = [
            IntentSignatureFields {
                from: "mallory",
                ..fields
            },
            IntentSignatureFields {
                to: "carol",
                ..fields
            },
            IntentSignatureFields {
                to_host: "host-c",
                ..fields
            },
            IntentSignatureFields {
                subject: Some("other"),
                ..fields
            },
            IntentSignatureFields {
                body: "HELLO",
                ..fields
            },
            IntentSignatureFields {
                idempotency_key: Some("event_2"),
                ..fields
            },
            IntentSignatureFields {
                priority: "normal",
                ..fields
            },
            IntentSignatureFields { ttl: 601, ..fields },
        ];
        for mutation in mutations {
            assert!(!verify_intent_v2(&pk, &sig, &mutation).unwrap());
        }
        assert!(!verify_intent_v2(
            &pk,
            &sig,
            &IntentSignatureFields {
                subject: None,
                ..fields
            },
        )
        .unwrap());
        assert!(!verify_intent_v2(
            &pk,
            &sig,
            &IntentSignatureFields {
                subject: Some(""),
                ..fields
            }
        )
        .unwrap());
        assert!(!verify_intent_v2(&pk, "v3:00", &fields).unwrap());
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

    /// The fingerprint is deterministic, correctly formatted (`SHA256:` + 16
    /// lowercase hex), and distinguishes distinct keys. The full digest is 64 hex.
    #[test]
    fn fingerprint_determinism_and_format() {
        let a = to_hex(test_key(3).verifying_key().as_bytes());
        let b = to_hex(test_key(4).verifying_key().as_bytes());

        let fa = fingerprint(&a).unwrap();
        let fa2 = fingerprint(&a).unwrap();
        assert_eq!(fa, fa2, "same pubkey ⇒ same fingerprint");
        assert_ne!(
            fingerprint(&b).unwrap(),
            fa,
            "different pubkey ⇒ different fp"
        );

        assert!(fa.starts_with("SHA256:"), "labeled with SHA256:");
        let short = fa.strip_prefix("SHA256:").unwrap();
        assert_eq!(short.len(), FINGERPRINT_DISPLAY_HEX, "16-hex display");
        assert!(
            short
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex only"
        );

        let full = fingerprint_full(&a).unwrap();
        assert_eq!(full.len(), 64, "full SHA-256 is 64 hex chars");
        assert!(
            full.starts_with(short),
            "display is a prefix of the full digest"
        );
    }

    /// A malformed/oversized pubkey yields `None` (never a panic), for every form.
    #[test]
    fn fingerprint_none_on_malformed() {
        assert!(fingerprint("").is_none(), "empty");
        assert!(fingerprint("zz").is_none(), "non-hex");
        assert!(
            fingerprint("deadbeef").is_none(),
            "too short (not 32 bytes)"
        );
        assert!(
            fingerprint(&"f".repeat(MAX_KEY_HEX_LEN + 2)).is_none(),
            "oversized"
        );
        assert!(fingerprint_full("abc").is_none(), "odd length");
    }

    /// Trust matching uses the FULL digest (R3): a `SHA256:<full-64-hex>` or a bare
    /// full-pubkey-hex entry matches; the truncated 16-hex display form NEVER does.
    #[test]
    fn fingerprint_matches_full_only() {
        let pk = to_hex(test_key(11).verifying_key().as_bytes());
        let full = fingerprint_full(&pk).unwrap();
        let display = fingerprint(&pk).unwrap(); // SHA256:<16-hex>

        assert!(
            fingerprint_matches(&format!("SHA256:{full}"), &pk),
            "full SHA256: matches"
        );
        assert!(
            fingerprint_matches(&format!("SHA256:{}", full.to_uppercase()), &pk),
            "case-insensitive"
        );
        assert!(
            fingerprint_matches(&pk, &pk),
            "bare full pubkey hex matches"
        );
        assert!(
            !fingerprint_matches(&display, &pk),
            "truncated display NEVER matches (R3)"
        );

        let other = to_hex(test_key(12).verifying_key().as_bytes());
        assert!(
            !fingerprint_matches(&format!("SHA256:{full}"), &other),
            "wrong key never matches"
        );
        assert!(
            !fingerprint_matches("garbage", &pk),
            "malformed entry never matches"
        );
    }

    use proptest::prelude::*;

    proptest! {
        /// `fingerprint`/`fingerprint_full` are TOTAL (Some/None without panic) on any
        /// hex-ish input and STABLE across calls; a `Some` always carries the label
        /// and 16-hex display whose prefix equals the full digest.
        #[test]
        fn fingerprint_total_and_stable(s in "[0-9a-fA-F]{0,300}") {
            let a = fingerprint(&s);
            let b = fingerprint(&s);
            prop_assert_eq!(&a, &b, "stable across calls");
            if let Some(fp) = a {
                prop_assert!(fp.starts_with("SHA256:"));
                let short = fp.strip_prefix("SHA256:").unwrap();
                prop_assert_eq!(short.len(), FINGERPRINT_DISPLAY_HEX);
                let full = fingerprint_full(&s).unwrap();
                prop_assert!(full.starts_with(short));
            }
        }

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

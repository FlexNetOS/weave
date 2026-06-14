//! Minimal, dependency-free USTAR (POSIX tar) writer/reader (WL-035).
//!
//! weave's `backup`/`restore` package a SQLite snapshot plus a couple of config
//! files into one portable archive. A real (if minimal) tar format gives us a
//! self-describing, `tar tf`-inspectable artifact with a well-understood
//! traversal-guard surface — and it needs **zero new dependencies**: a tar entry
//! is a 512-byte header block (name, mode, octal size, checksum, typeflag) followed
//! by the file body zero-padded to a 512-byte boundary, terminated by two zero
//! blocks. We implement only the regular-file (`typeflag '0'`) subset we need.
//!
//! This module is PURE: it operates on byte buffers only (no filesystem, no I/O),
//! so it unit-tests in isolation and sits at the `model` layer of the DAG.

use anyhow::{bail, Result};

/// The fixed manifest of entry names a weave archive may contain. The extractor
/// rejects any entry name not in this set (traversal/poisoning guard): a restore
/// must never write a file the archive author did not intend.
pub const ENTRY_DB: &str = "messages.db";
pub const ENTRY_CONFIG: &str = "config.toml";
pub const ENTRY_SETTINGS: &str = "settings.json";
pub const ENTRY_MANIFEST: &str = "MANIFEST";

/// Every legal entry name in a weave archive. `safe_entry_name` accepts ONLY
/// these (in addition to its structural checks). Adding a new archive member means
/// adding its constant here.
pub const KNOWN_ENTRY_NAMES: &[&str] = &[ENTRY_DB, ENTRY_CONFIG, ENTRY_SETTINGS, ENTRY_MANIFEST];

/// A single archive member: a name and its raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub name: String,
    pub data: Vec<u8>,
}

const BLOCK: usize = 512;

/// Reject any entry name that is unsafe to extract. This is the security-critical
/// traversal guard (HARD CONSTRAINT): an archive is untrusted input.
///
/// Rejects: empty names, absolute paths, any `..` path component, any embedded
/// `/` or `\` (our manifest names are flat), any embedded NUL, names longer than
/// the 100-byte USTAR `name` field, and — the strongest check — any name that is
/// not one of the [`KNOWN_ENTRY_NAMES`]. Because the accept-list is a closed set
/// of flat constants, a malicious name can never slip through.
pub fn safe_entry_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("archive entry name is empty");
    }
    if name.len() > 100 {
        bail!("archive entry name exceeds the 100-byte USTAR limit: {name:?}");
    }
    if name.contains('\0') {
        bail!("archive entry name contains a NUL byte: {name:?}");
    }
    if name.starts_with('/') || name.starts_with('\\') {
        bail!("archive entry name is absolute: {name:?}");
    }
    if name.contains('/') || name.contains('\\') {
        bail!("archive entry name contains a path separator: {name:?}");
    }
    // A Windows drive-letter absolute (e.g. `C:\`) — reject any colon defensively.
    if name.contains(':') {
        bail!("archive entry name contains a drive/colon: {name:?}");
    }
    if name == "." || name == ".." {
        bail!("archive entry name is a relative path component: {name:?}");
    }
    if !KNOWN_ENTRY_NAMES.contains(&name) {
        bail!("archive entry name is not a known weave member: {name:?}");
    }
    Ok(())
}

/// Build an uncompressed USTAR archive from the given `(name, bytes)` entries.
/// Entry names must fit the 100-byte USTAR `name` field; we assert that here so a
/// future variable-length entry name can never silently overflow. Returns the full
/// archive bytes (including the two trailing zero blocks).
pub fn write_archive(entries: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (name, data) in entries {
        if name.len() > 100 {
            bail!("archive entry name exceeds the 100-byte USTAR limit: {name:?}");
        }
        if name.contains('\0') {
            bail!("archive entry name contains a NUL byte: {name:?}");
        }
        out.extend_from_slice(&header_block(name, data.len())?);
        out.extend_from_slice(data);
        // Zero-pad the body to the next 512-byte boundary.
        let rem = data.len() % BLOCK;
        if rem != 0 {
            out.extend(std::iter::repeat_n(0u8, BLOCK - rem));
        }
    }
    // End-of-archive marker: two zero blocks.
    out.extend(std::iter::repeat_n(0u8, BLOCK * 2));
    Ok(out)
}

/// Parse an uncompressed USTAR archive produced by [`write_archive`]. Validates
/// each header's checksum, reads `size` bytes of body, and stops at the zero-block
/// terminator. Returns an error (never a panic) on a truncated or corrupt buffer.
/// Note: this does NOT apply [`safe_entry_name`] — the extractor must call that on
/// every returned entry before doing anything with it.
pub fn read_archive(buf: &[u8]) -> Result<Vec<ArchiveEntry>> {
    let mut entries = Vec::new();
    let mut pos = 0usize;
    loop {
        if pos + BLOCK > buf.len() {
            bail!("archive truncated: expected a 512-byte header at offset {pos}");
        }
        let header = &buf[pos..pos + BLOCK];
        // A zero block marks the end of the archive.
        if header.iter().all(|&b| b == 0) {
            break;
        }
        // Verify the header checksum: the sum of all header bytes with the 8
        // checksum bytes treated as spaces (0x20), compared against the octal
        // value stored in the checksum field (bytes 148..156).
        let stored = parse_octal(&header[148..156])
            .ok_or_else(|| anyhow::anyhow!("archive header has an invalid checksum field"))?;
        let computed = checksum(header);
        if stored != computed {
            bail!(
                "archive header checksum mismatch (stored {stored}, computed {computed}) \
                 at offset {pos}"
            );
        }
        // typeflag at byte 156: only regular files ('0' or NUL) are supported.
        let typeflag = header[156];
        if typeflag != b'0' && typeflag != 0 {
            bail!("archive entry has unsupported typeflag {typeflag:#x} at offset {pos}");
        }
        let name = parse_name(&header[0..100]);
        let size = parse_octal(&header[124..136])
            .ok_or_else(|| anyhow::anyhow!("archive header has an invalid size field"))?
            as usize;
        pos += BLOCK;
        if pos + size > buf.len() {
            bail!("archive truncated: body of {name:?} ({size} bytes) runs past the buffer");
        }
        let data = buf[pos..pos + size].to_vec();
        // Advance past the zero-padded body.
        let padded = size.div_ceil(BLOCK) * BLOCK;
        pos += padded;
        entries.push(ArchiveEntry { name, data });
    }
    Ok(entries)
}

/// Build a single 512-byte USTAR header block for a regular file.
fn header_block(name: &str, size: usize) -> Result<[u8; BLOCK]> {
    let mut h = [0u8; BLOCK];
    // name (0..100)
    let nb = name.as_bytes();
    h[0..nb.len()].copy_from_slice(nb);
    // mode (100..108), octal, NUL-terminated. 0644.
    write_octal_field(&mut h[100..108], 0o644);
    // uid (108..116), gid (116..124): leave as octal 0.
    write_octal_field(&mut h[108..116], 0);
    write_octal_field(&mut h[116..124], 0);
    // size (124..136), octal.
    write_octal_field(&mut h[124..136], size as u64);
    // mtime (136..148), octal 0 (portability container, not a timestamp record).
    write_octal_field(&mut h[136..148], 0);
    // typeflag (156): regular file.
    h[156] = b'0';
    // magic (257..263) "ustar\0", version (263..265) "00".
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    // checksum (148..156): computed with the field initialized to spaces, then
    // written as 6 octal digits, a NUL, and a space (the canonical layout).
    for b in &mut h[148..156] {
        *b = b' ';
    }
    let sum = checksum(&h);
    let cs = format!("{sum:06o}");
    h[148..148 + cs.len()].copy_from_slice(cs.as_bytes());
    h[148 + cs.len()] = 0;
    h[155] = b' ';
    Ok(h)
}

/// Sum of every byte in the header with the 8 checksum bytes (148..156) counted as
/// ASCII spaces. Shared by writer and reader so they cannot diverge.
fn checksum(header: &[u8]) -> u64 {
    let mut sum: u64 = 0;
    for (i, &b) in header.iter().enumerate() {
        if (148..156).contains(&i) {
            sum += u64::from(b' ');
        } else {
            sum += u64::from(b);
        }
    }
    sum
}

/// Write a right-justified, NUL-padded octal numeric field (USTAR style): the
/// value as octal digits followed by a single NUL, left-padded with `0`. Width is
/// the slice length; assumes the value fits (our values do).
fn write_octal_field(field: &mut [u8], value: u64) {
    let digits = field.len() - 1; // reserve the trailing NUL
    let s = format!("{value:0width$o}", width = digits);
    let bytes = s.as_bytes();
    // Take the last `digits` chars (truncate high zeros if somehow longer).
    let start = bytes.len().saturating_sub(digits);
    field[..digits].copy_from_slice(&bytes[start..]);
    field[digits] = 0;
}

/// Parse a NUL/space-padded octal field into a number, or `None` if it is not a
/// valid octal value.
fn parse_octal(field: &[u8]) -> Option<u64> {
    let mut value: u64 = 0;
    let mut saw_digit = false;
    for &b in field {
        match b {
            b'0'..=b'7' => {
                value = value.checked_mul(8)?.checked_add(u64::from(b - b'0'))?;
                saw_digit = true;
            }
            b' ' | 0 => {
                // Padding/terminator: stop on the first one after any digits, but
                // also tolerate leading spaces before digits.
                if saw_digit {
                    break;
                }
            }
            _ => return None,
        }
    }
    Some(value)
}

/// Parse the NUL-terminated `name` field into an owned `String` (lossy on any
/// non-UTF-8 bytes, which our writer never produces).
fn parse_name(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_entries() {
        let db = vec![1u8, 2, 3, 4, 5];
        let cfg = b"key = \"value\"\n".to_vec();
        let entries: &[(&str, &[u8])] = &[(ENTRY_DB, &db), (ENTRY_CONFIG, &cfg)];
        let bytes = write_archive(entries).unwrap();
        let back = read_archive(&bytes).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, ENTRY_DB);
        assert_eq!(back[0].data, db);
        assert_eq!(back[1].name, ENTRY_CONFIG);
        assert_eq!(back[1].data, cfg);
    }

    #[test]
    fn round_trip_empty_body() {
        let entries: &[(&str, &[u8])] = &[(ENTRY_MANIFEST, b"")];
        let bytes = write_archive(entries).unwrap();
        let back = read_archive(&bytes).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, ENTRY_MANIFEST);
        assert!(back[0].data.is_empty());
    }

    #[test]
    fn round_trip_block_aligned_body() {
        // Exactly 512 bytes: ensures no spurious extra padding block.
        let body = vec![7u8; 512];
        let entries: &[(&str, &[u8])] = &[(ENTRY_DB, &body)];
        let bytes = write_archive(entries).unwrap();
        let back = read_archive(&bytes).unwrap();
        assert_eq!(back[0].data, body);
    }

    #[test]
    fn absent_member_simply_omitted() {
        // An archive may carry only the DB (config/settings absent).
        let db = vec![9u8; 100];
        let entries: &[(&str, &[u8])] = &[(ENTRY_DB, &db)];
        let bytes = write_archive(entries).unwrap();
        let back = read_archive(&bytes).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, ENTRY_DB);
    }

    #[test]
    fn truncated_buffer_is_rejected_not_panicked() {
        let db = vec![1u8; 600];
        let entries: &[(&str, &[u8])] = &[(ENTRY_DB, &db)];
        let mut bytes = write_archive(entries).unwrap();
        bytes.truncate(700); // cut into the middle of the body
        assert!(read_archive(&bytes).is_err());
    }

    #[test]
    fn corrupted_checksum_is_rejected() {
        let db = vec![3u8; 64];
        let entries: &[(&str, &[u8])] = &[(ENTRY_DB, &db)];
        let mut bytes = write_archive(entries).unwrap();
        // Flip a byte inside the first header's name field to break the checksum.
        bytes[5] ^= 0xff;
        assert!(read_archive(&bytes).is_err());
    }

    #[test]
    fn traversal_guard_rejects_dangerous_names() {
        assert!(safe_entry_name("../etc/passwd").is_err());
        assert!(safe_entry_name("/etc/passwd").is_err());
        assert!(safe_entry_name("a/../../b").is_err());
        assert!(safe_entry_name("..").is_err());
        assert!(safe_entry_name(".").is_err());
        assert!(safe_entry_name("dir/file").is_err());
        assert!(safe_entry_name("back\\slash").is_err());
        assert!(safe_entry_name("C:\\evil").is_err());
        assert!(safe_entry_name("with\0nul").is_err());
        assert!(safe_entry_name("").is_err());
        assert!(safe_entry_name("unknown.txt").is_err());
    }

    #[test]
    fn traversal_guard_accepts_known_members() {
        assert!(safe_entry_name(ENTRY_DB).is_ok());
        assert!(safe_entry_name(ENTRY_CONFIG).is_ok());
        assert!(safe_entry_name(ENTRY_SETTINGS).is_ok());
        assert!(safe_entry_name(ENTRY_MANIFEST).is_ok());
    }

    #[test]
    fn read_back_validates_every_entry_name() {
        // A hand-built archive with an evil name parses (read_archive does not
        // filter) but is caught when the extractor applies safe_entry_name.
        let evil = [0u8; 16];
        let bytes = write_archive(&[("messages.db", &evil[..])]).unwrap();
        let back = read_archive(&bytes).unwrap();
        for e in &back {
            assert!(safe_entry_name(&e.name).is_ok());
        }
    }
}

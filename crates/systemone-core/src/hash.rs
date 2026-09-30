//! Shared SHA-256 helpers.
//!
//! Every place that verifies a model file against a pinned digest hashes it
//! the same way: read the whole file in chunks, then render the digest as
//! lowercase hex. Keeping this in one place stops the download check and the
//! runtime check from drifting apart.

use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

use sha2::{Digest, Sha256};

const CHUNK_BYTES: usize = 1024 * 1024;

/// SHA-256 of a file: the bytes read and the lowercase hex digest.
pub fn sha256_file(path: &Path) -> io::Result<(u64, String)> {
    let mut input = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    let mut total = 0_u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((total, hex(&hasher.finalize())))
}

/// Bytes as lowercase hex.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_a_file_and_reports_its_length() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("f");
        std::fs::write(&path, b"alpha").unwrap();
        let (bytes, digest) = sha256_file(&path).unwrap();
        assert_eq!(bytes, 5);
        assert_eq!(digest, hex(&Sha256::digest(b"alpha")));
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_missing_file_is_an_io_error() {
        let directory = tempfile::tempdir().unwrap();
        assert!(sha256_file(&directory.path().join("missing")).is_err());
    }
}

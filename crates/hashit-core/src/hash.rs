use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum HashAlgo {
    /// Fast, parallel-friendly cryptographic hash (default).
    Blake3,
    /// Ubiquitous SHA-256, for interop with standard tooling.
    Sha256,
}

impl HashAlgo {
    pub fn name(self) -> &'static str {
        match self {
            HashAlgo::Blake3 => "blake3",
            HashAlgo::Sha256 => "sha256",
        }
    }
}

const CHUNK: usize = 64 * 1024;

/// Stream a file through the chosen hash and return the lowercase hex digest.
pub fn hash_file(path: &Path, algo: HashAlgo) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = vec![0u8; CHUNK];
    match algo {
        HashAlgo::Blake3 => {
            let mut hasher = blake3::Hasher::new();
            loop {
                let n = f.read(&mut buf).with_context(|| format!("reading {}", path.display()))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            Ok(hasher.finalize().to_hex().to_string())
        }
        HashAlgo::Sha256 => {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            loop {
                let n = f.read(&mut buf).with_context(|| format!("reading {}", path.display()))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            Ok(hex::encode(hasher.finalize()))
        }
    }
}

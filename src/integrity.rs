//! Expected-checksum parsing and verification.

use std::fmt;

/// A trusted expected digest for the final file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checksum {
    sha256: [u8; 32],
}

impl Checksum {
    /// Parse `sha256:HEX` (64 hexadecimal digits, case-insensitive).
    pub fn parse(input: &str) -> Result<Checksum, String> {
        let input = input.trim();
        let Some((algorithm, hex)) = input.split_once(':') else {
            return Err("expected `sha256:HEX`".into());
        };
        if !algorithm.eq_ignore_ascii_case("sha256") {
            return Err(format!(
                "unsupported checksum algorithm `{algorithm}`; use sha256"
            ));
        }
        let sha256 = decode_hex(hex)
            .ok_or_else(|| "a SHA-256 digest is 64 hexadecimal characters".to_string())?;
        Ok(Checksum { sha256 })
    }

    pub fn matches(&self, digest: &[u8; 32]) -> bool {
        // Not secret material; a plain comparison is fine.
        self.sha256 == *digest
    }

    pub fn hex(&self) -> String {
        hex(&self.sha256)
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.hex())
    }
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn decode_hex(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.as_bytes();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in hex.as_chunks::<2>().0.iter().enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn parse_and_match() {
        let checksum = Checksum::parse(&format!("sha256:{EMPTY}")).unwrap();
        let digest: [u8; 32] = Sha256::digest(b"").into();
        assert!(checksum.matches(&digest));
        assert_eq!(checksum.hex(), EMPTY);
        assert!(Checksum::parse(&format!("SHA256:{}", EMPTY.to_uppercase())).is_ok());
    }

    #[test]
    fn rejects_malformed() {
        assert!(Checksum::parse(EMPTY).is_err());
        assert!(Checksum::parse(&format!("md5:{EMPTY}")).is_err());
        assert!(Checksum::parse("sha256:abcd").is_err());
        assert!(Checksum::parse(&format!("sha256:{}zz", &EMPTY[..62])).is_err());
    }
}

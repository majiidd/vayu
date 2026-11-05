//! Versioned transfer state (`NAME.vayu`) and its durable publication.
//!
//! Layout: one header line, `vayu-state 1 <SHA-256 of the body>`, followed by a JSON body. The
//! digest detects torn or damaged writes, and every field is bounds-checked before recovery
//! trusts an offset or allocates anything.
//!
//! The state records what a later run needs to continue safely: a request identity (a hash of
//! the URL, so signed query secrets are not stored), the representation's size and validators,
//! an optional expected checksum, the byte ranges that are durably on disk, and a SHA-256 of the
//! saved bytes of each block. Credentials, headers, and cookies are never stored.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::fault;
use crate::integrity::{Checksum, hex};
use crate::transport::http::Validators;

pub const FORMAT_VERSION: u32 = 1;
const MAGIC: &str = "vayu-state";
/// Upper bound on a state file; larger files are rejected before parsing.
pub const MAX_STATE_BYTES: u64 = 8 << 20;
pub const MAX_BLOCKS: u64 = 1 << 16;
const MAX_RANGES: usize = 1 << 16;
const MAX_TEXT: usize = 4096;
const MIN_BLOCK_SIZE: u64 = 64 << 10;
const MAX_BLOCK_SIZE: u64 = 1 << 30;
/// Block sizing for new downloads: about this many blocks, at least 1 MiB each.
const TARGET_BLOCKS: u64 = 4096;
const DEFAULT_BLOCK_SIZE: u64 = 1 << 20;
const UNKNOWN_SIZE_BLOCK: u64 = 16 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// Random identifier of this download, kept across runs.
    pub id: String,
    /// Incremented on every publication.
    pub generation: u64,
    /// The final file name; the state lives next to it as `NAME.vayu`.
    pub file_name: String,
    pub source: Source,
    pub total: Option<u64>,
    pub validators: Validators,
    /// `sha256:HEX` supplied by the user; gates publication.
    pub checksum: Option<String>,
    pub block_size: u64,
    /// Durably saved byte ranges, `[start, end)`, sorted and non-overlapping.
    pub ranges: Vec<[u64; 2]>,
    /// SHA-256 of the saved bytes of each block, which must start at the block's first byte.
    /// `None` for blocks with nothing verifiable.
    pub blocks: Vec<Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// [`request_identity`] of the URL the user gave.
    pub request: String,
    /// The URL's origin and last path segment, for diagnostics only. Signed links can carry
    /// secrets in any other part.
    pub url: String,
}

/// Why a state file cannot be used.
#[derive(Debug)]
pub enum LoadError {
    Io(io::Error),
    Invalid(String),
}

impl State {
    pub fn new(
        url: &Url,
        file_name: String,
        total: Option<u64>,
        validators: Validators,
        checksum: Option<&Checksum>,
    ) -> State {
        State {
            id: format!("{:032x}", fastrand::u128(..)),
            generation: 0,
            file_name,
            source: Source::of(url),
            total,
            validators,
            checksum: checksum.map(Checksum::to_string),
            block_size: block_size_for(total),
            ranges: Vec::new(),
            blocks: Vec::new(),
        }
    }

    /// Length of the saved run starting at offset zero.
    pub fn prefix(&self) -> u64 {
        match self.ranges.first() {
            Some(&[0, end]) => end,
            _ => 0,
        }
    }

    pub fn checksum(&self) -> Option<Checksum> {
        // Validated on load.
        self.checksum
            .as_deref()
            .and_then(|c| Checksum::parse(c).ok())
    }

    pub fn encode(&self) -> Vec<u8> {
        let body = serde_json::to_vec(self).expect("state serializes");
        let digest: [u8; 32] = Sha256::digest(&body).into();
        let mut out = format!("{MAGIC} {FORMAT_VERSION} {}\n", hex(&digest)).into_bytes();
        out.extend_from_slice(&body);
        out.push(b'\n');
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<State, String> {
        let newline = bytes
            .iter()
            .position(|&b| b == b'\n')
            .ok_or("not a vayu state file")?;
        let header = std::str::from_utf8(&bytes[..newline]).map_err(|_| "not a vayu state file")?;
        let mut fields = header.split(' ');
        if fields.next() != Some(MAGIC) {
            return Err("not a vayu state file".into());
        }
        let version = fields.next().unwrap_or_default();
        if version != FORMAT_VERSION.to_string() {
            return Err(format!(
                "unsupported state format version {version} (this vayu reads version \
                 {FORMAT_VERSION})"
            ));
        }
        let expected = fields.next().unwrap_or_default();
        let body = &bytes[newline + 1..];
        let body = body.strip_suffix(b"\n").unwrap_or(body);
        let digest: [u8; 32] = Sha256::digest(body).into();
        if fields.next().is_some() || expected != hex(&digest) {
            return Err("the state file is damaged (its checksum does not match)".into());
        }
        let state: State =
            serde_json::from_slice(body).map_err(|e| format!("invalid state: {e}"))?;
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> Result<(), String> {
        let invalid = |what: &str| Err(format!("invalid state: {what}"));
        if self.id.len() != 32 || !is_hex(&self.id) {
            return invalid("download id");
        }
        let name = &self.file_name;
        if name.is_empty()
            || name.len() > 255
            || name.contains(['/', '\0'])
            || name == "."
            || name == ".."
        {
            return invalid("file name");
        }
        if self.source.request.len() != 64 || !is_hex(&self.source.request) {
            return invalid("request identity");
        }
        let v = &self.validators;
        let texts = [
            &v.etag,
            &v.strong_etag,
            &v.strong_last_modified,
            &self.checksum,
        ];
        if self.source.url.len() > MAX_TEXT
            || texts
                .iter()
                .any(|t| t.as_ref().is_some_and(|t| t.len() > MAX_TEXT))
        {
            return invalid("oversized text field");
        }
        if let Some(c) = &self.checksum
            && Checksum::parse(c).is_err()
        {
            return invalid("checksum");
        }
        if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&self.block_size) {
            return invalid("block size");
        }
        if self.ranges.len() > MAX_RANGES {
            return invalid("too many ranges");
        }
        let mut previous_end = 0;
        for (i, &[start, end]) in self.ranges.iter().enumerate() {
            if start >= end || (i > 0 && start < previous_end) {
                return invalid("overlapping or empty ranges");
            }
            if !start.is_multiple_of(self.block_size) {
                return invalid("range that does not start at a block boundary");
            }
            previous_end = end;
        }
        let limit = self
            .total
            .unwrap_or(u64::MAX)
            .min(self.block_size.saturating_mul(MAX_BLOCKS));
        if previous_end > limit {
            return invalid("range beyond the end of the file");
        }
        if self.blocks.len() as u64 != previous_end.div_ceil(self.block_size) {
            return invalid("block count does not match the ranges");
        }
        for (i, hash) in self.blocks.iter().enumerate() {
            let Some(hash) = hash else { continue };
            if hash.len() != 64 || !is_hex(hash) {
                return invalid("block hash");
            }
            if !self.block_starts_covered(i as u64) {
                return invalid("hash for a block whose start is not saved");
            }
        }
        Ok(())
    }

    fn block_starts_covered(&self, index: u64) -> bool {
        let start = index * self.block_size;
        let next = self.ranges.partition_point(|&[_, end]| end <= start);
        self.ranges.get(next).is_some_and(|&[s, _]| s <= start)
    }
}

impl Source {
    pub fn of(url: &Url) -> Source {
        Source {
            request: request_identity(url),
            url: describe(url),
        }
    }
}

/// A non-reversible identity for automatically matching a rerun to its saved progress.
pub fn request_identity(url: &Url) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"vayu-request-v1\n");
    hasher.update(url.as_str().as_bytes());
    hex(&hasher.finalize())
}

/// `https://example.com/…/file.zip`: enough to recognize a download, nothing more.
fn describe(url: &Url) -> String {
    let origin = url.origin().ascii_serialization();
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        [] => format!("{origin}/"),
        [only] => format!("{origin}/{only}"),
        [.., last] => format!("{origin}/…/{last}"),
    }
}

fn block_size_for(total: Option<u64>) -> u64 {
    match total {
        Some(total) => (total / TARGET_BLOCKS)
            .next_power_of_two()
            .clamp(DEFAULT_BLOCK_SIZE, MAX_BLOCK_SIZE),
        None => UNKNOWN_SIZE_BLOCK,
    }
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read and validate a state file without trusting its size.
pub fn load(path: &Path) -> Result<State, LoadError> {
    let file = File::open(path).map_err(LoadError::Io)?;
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(LoadError::Io)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(LoadError::Invalid("the state file is too large".into()));
    }
    State::decode(&bytes).map_err(LoadError::Invalid)
}

/// Replace the state file atomically: write a synchronized temporary file next to it, rename it
/// over the old one, then synchronize the directory. Readable and writable by the owner only.
pub fn publish(state: &State, path: &Path, tmp: &Path) -> io::Result<()> {
    let bytes = state.encode();
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(tmp)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fault::point("checkpoint-tmp-written");
    fs::rename(tmp, path)?;
    if let Some(parent) = path.parent() {
        crate::storage::sync_dir(parent);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> State {
        let url = Url::parse("https://example.com/f.iso?sig=secret").unwrap();
        let mut state = State::new(
            &url,
            "f.iso".into(),
            Some(3 << 20),
            Validators::default(),
            None,
        );
        state.ranges = vec![[0, (2 << 20) + 5]];
        state.blocks = vec![
            Some("a".repeat(64)),
            Some("b".repeat(64)),
            Some("c".repeat(64)),
        ];
        state
    }

    #[test]
    fn round_trips_without_secrets() {
        let state = sample();
        let bytes = state.encode();
        assert!(bytes.starts_with(b"vayu-state 1 "));
        assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
        assert_eq!(State::decode(&bytes).unwrap(), state);
        assert_eq!(state.prefix(), (2 << 20) + 5);
        assert_eq!(state.block_size, 1 << 20);
    }

    #[test]
    fn stored_url_keeps_only_origin_and_name() {
        let url = Url::parse("https://u:pw@example.com/t0ken/dl/f.iso?sig=x#frag").unwrap();
        assert_eq!(describe(&url), "https://example.com/…/f.iso");
        let url = Url::parse("http://example.com:8080/f.iso").unwrap();
        assert_eq!(describe(&url), "http://example.com:8080/f.iso");
    }

    #[test]
    fn identity_depends_on_the_whole_url() {
        let a = Url::parse("https://example.com/f?sig=1").unwrap();
        let b = Url::parse("https://example.com/f?sig=2").unwrap();
        assert_ne!(request_identity(&a), request_identity(&b));
        assert_eq!(request_identity(&a), request_identity(&a.clone()));
    }

    #[test]
    fn detects_damage_and_unknown_versions() {
        let mut bytes = sample().encode();
        let at = bytes.len() - 10;
        bytes[at] ^= 1;
        assert!(State::decode(&bytes).unwrap_err().contains("damaged"));

        let body = sample().encode();
        let v2 = String::from_utf8(body).unwrap().replacen(" 1 ", " 2 ", 1);
        assert!(
            State::decode(v2.as_bytes())
                .unwrap_err()
                .contains("version 2")
        );
        assert!(State::decode(b"hello\n{}").is_err());
        assert!(State::decode(b"").is_err());
    }

    #[test]
    fn rejects_out_of_bounds_layouts() {
        let check = |f: &dyn Fn(&mut State)| {
            let mut state = sample();
            f(&mut state);
            State::decode(&state.encode())
        };
        assert!(check(&|_| {}).is_ok());
        assert!(
            check(&|s| s.ranges = vec![[0, 4 << 20]]).is_err(),
            "beyond total"
        );
        assert!(
            check(&|s| s.ranges = vec![[0, 10], [5, 20]]).is_err(),
            "overlap"
        );
        assert!(check(&|s| s.ranges = vec![[7, 7]]).is_err(), "empty");
        assert!(
            check(&|s| {
                s.ranges = vec![[0, 10], [(1 << 20) + 5, 2 << 20]];
                s.blocks.pop();
            })
            .is_err(),
            "unaligned range start"
        );
        assert!(
            check(&|s| {
                s.ranges = vec![[0, 10], [2 << 20, (2 << 20) + 5]];
                s.blocks[1] = None;
            })
            .is_ok(),
            "separate ranges"
        );
        assert!(check(&|s| s.blocks.push(None)).is_err(), "block count");
        assert!(check(&|s| s.blocks[0] = Some("zz".into())).is_err());
        assert!(check(&|s| s.block_size = 1).is_err());
        assert!(check(&|s| s.file_name = "../x".into()).is_err());
        assert!(check(&|s| s.checksum = Some("md5:00".into())).is_err());
        assert!(
            check(&|s| {
                s.total = None;
                s.ranges = vec![[0, u64::MAX]];
            })
            .is_err(),
            "unbounded size"
        );
        assert!(
            check(&|s| {
                s.ranges = vec![[1, (2 << 20) + 5]];
            })
            .is_err(),
            "hash for a block whose start is missing"
        );
    }

    #[test]
    fn publication_is_owner_only_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.iso.vayu");
        let tmp = dir.path().join("f.iso.vayu.tmp");
        let mut state = sample();
        publish(&state, &path, &tmp).unwrap();
        state.generation = 7;
        publish(&state, &path, &tmp).unwrap();
        assert!(!tmp.exists());
        assert_eq!(load(&path).unwrap().generation, 7);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

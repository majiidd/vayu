//! Hashes of the partial file: one SHA-256 per block, and a running SHA-256 of the whole file.
//!
//! The per-block hashes go into each checkpoint, so a later run can detect accidental damage to
//! saved data. They prove nothing about the remote source. Every block is written in order from
//! its first byte (the scheduler splits work only at block boundaries), so a partly written
//! block's hash covers exactly its saved bytes, which keeps checkpoints byte-granular on slow
//! links.
//!
//! The whole-file hash trails the contiguous prefix. Data that arrives exactly where the hash
//! stands is hashed in memory, as in a single-stream download; data that other connections wrote
//! further ahead is read back from the file once the gap before it has closed.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use sha2::{Digest, Sha256};

use crate::integrity::hex;
use crate::state::State;

const READ_BUFFER: usize = 1 << 20;

#[derive(Clone)]
enum Block {
    /// `len` bytes from the block's first byte, possibly none.
    Partial {
        len: u64,
        hasher: Sha256,
    },
    Full(String),
}

impl Block {
    fn empty() -> Block {
        Block::Partial {
            len: 0,
            hasher: Sha256::new(),
        }
    }

    fn len(&self, block_size: u64) -> u64 {
        match self {
            Block::Partial { len, .. } => *len,
            Block::Full(_) => block_size,
        }
    }

    fn hash(&self) -> Option<String> {
        match self {
            Block::Partial { len: 0, .. } => None,
            Block::Partial { hasher, .. } => Some(hex(&hasher.clone().finalize())),
            Block::Full(hash) => Some(hash.clone()),
        }
    }
}

#[derive(Clone)]
pub struct Ledger {
    block_size: u64,
    blocks: Vec<Block>,
    /// Bytes saved, in all blocks.
    bytes: u64,
    /// SHA-256 of `[0, hashed)`.
    whole: Sha256,
    hashed: u64,
}

impl Ledger {
    pub fn new(block_size: u64) -> Ledger {
        Ledger {
            block_size,
            blocks: Vec::new(),
            bytes: 0,
            whole: Sha256::new(),
            hashed: 0,
        }
    }

    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    /// Bytes saved, in all ranges.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Length of the saved run starting at offset zero.
    pub fn prefix(&self) -> u64 {
        match self.ranges().first() {
            Some(&[0, end]) => end,
            _ => 0,
        }
    }

    /// Account for `data` written at `offset`. Each block must be continued exactly where its
    /// saved bytes end; anything else is an internal error, and nothing is recorded.
    pub fn record(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let b = self.block_size;
        let last = (offset + data.len() as u64).div_ceil(b) as usize;
        if self.blocks.len() < last {
            self.blocks.resize(last, Block::empty());
        }
        let mut at = offset;
        let mut rest = data;
        // Check every piece before changing anything.
        while !rest.is_empty() {
            let index = (at / b) as usize;
            let within = at % b;
            if self.blocks[index].len(b) != within {
                return Err(io::Error::other(format!(
                    "internal error: write at offset {at} does not continue its block"
                )));
            }
            let take = rest.len().min((b - within) as usize);
            at += take as u64;
            rest = &rest[take..];
        }
        let mut at = offset;
        let mut rest = data;
        while !rest.is_empty() {
            let index = (at / b) as usize;
            let take = rest.len().min((b - at % b) as usize);
            let (piece, tail) = rest.split_at(take);
            if let Block::Partial { len, hasher } = &mut self.blocks[index] {
                hasher.update(piece);
                *len += take as u64;
                if *len == b {
                    let done = std::mem::take(hasher).finalize();
                    self.blocks[index] = Block::Full(hex(&done));
                }
            }
            at += take as u64;
            rest = tail;
        }
        self.bytes += data.len() as u64;
        if offset == self.hashed {
            self.whole.update(data);
            self.hashed += data.len() as u64;
        }
        Ok(())
    }

    /// Saved byte ranges, `[start, end)`, sorted and disjoint. Every range starts at a block
    /// boundary.
    pub fn ranges(&self) -> Vec<[u64; 2]> {
        let mut ranges: Vec<[u64; 2]> = Vec::new();
        for (index, block) in self.blocks.iter().enumerate() {
            let len = block.len(self.block_size);
            if len == 0 {
                continue;
            }
            let start = index as u64 * self.block_size;
            match ranges.last_mut() {
                Some(last) if last[1] == start => last[1] = start + len,
                _ => ranges.push([start, start + len]),
            }
        }
        ranges
    }

    /// Block hashes for a checkpoint covering [`Ledger::ranges`].
    pub fn hashes(&self) -> Vec<Option<String>> {
        let used = self
            .blocks
            .iter()
            .rposition(|b| b.len(self.block_size) > 0)
            .map_or(0, |i| i + 1);
        self.blocks[..used].iter().map(Block::hash).collect()
    }

    /// Extend the whole-file hash over the contiguous prefix by reading back what other
    /// connections wrote ahead of it. All written data must have reached `file`.
    pub fn catch_up(&mut self, file: &File) -> io::Result<()> {
        let prefix = self.prefix();
        if self.hashed >= prefix {
            return Ok(());
        }
        let mut buffer = vec![0u8; READ_BUFFER.min((prefix - self.hashed) as usize)];
        while self.hashed < prefix {
            let n = buffer.len().min((prefix - self.hashed) as usize);
            file.read_exact_at(&mut buffer[..n], self.hashed)?;
            self.whole.update(&buffer[..n]);
            self.hashed += n as u64;
        }
        Ok(())
    }

    /// SHA-256 of the prefix covered so far; after [`Ledger::catch_up`], of the whole prefix.
    pub fn digest(&self) -> [u8; 32] {
        self.whole.clone().finalize().into()
    }

    /// Re-read the saved data of `file` and keep, in each saved range, the longest run of blocks
    /// whose hashes match `state`. Claims beyond the end of the file count as missing. Nothing is
    /// modified.
    pub fn recover(file: &File, state: &State) -> io::Result<Ledger> {
        let b = state.block_size;
        let len = file.metadata()?.len();
        let mut ledger = Ledger::new(b);
        let mut buffer = vec![0u8; READ_BUFFER];
        for &[start, end] in &state.ranges {
            let mut at = start;
            while at < end {
                let index = (at / b) as usize;
                let stop = ((index as u64 + 1) * b).min(end);
                let Some(Some(expected)) = state.blocks.get(index) else {
                    break;
                };
                if stop > len {
                    break;
                }
                let contiguous = at == ledger.hashed;
                let mut whole = ledger.whole.clone();
                let mut block = Sha256::new();
                let mut offset = at;
                while offset < stop {
                    let n = buffer.len().min((stop - offset) as usize);
                    file.read_exact_at(&mut buffer[..n], offset)?;
                    block.update(&buffer[..n]);
                    if contiguous {
                        whole.update(&buffer[..n]);
                    }
                    offset += n as u64;
                }
                if hex(&block.clone().finalize()) != *expected {
                    break;
                }
                if ledger.blocks.len() <= index {
                    ledger.blocks.resize(index + 1, Block::empty());
                }
                let want = stop - at;
                ledger.blocks[index] = if want == b {
                    Block::Full(expected.clone())
                } else {
                    Block::Partial {
                        len: want,
                        hasher: block,
                    }
                };
                ledger.bytes += want;
                if contiguous {
                    ledger.whole = whole;
                    ledger.hashed = stop;
                }
                at = stop;
            }
        }
        Ok(ledger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::http::Validators;
    use std::fs::OpenOptions;

    const BLOCK: u64 = 1 << 20;

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn state_for(ledger: &Ledger, total: u64) -> State {
        let url = url::Url::parse("https://example.com/x").unwrap();
        let mut state = State::new(&url, "x".into(), Some(total), Validators::default(), None);
        assert_eq!(state.block_size, BLOCK);
        state.ranges = ledger.ranges();
        state.blocks = ledger.hashes();
        state
    }

    fn file_with(dir: &tempfile::TempDir, bytes: &[u8]) -> File {
        let path = dir.path().join("x.part");
        std::fs::write(&path, bytes).unwrap();
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap()
    }

    #[test]
    fn chunking_does_not_change_hashes() {
        let bytes = data(2 * BLOCK as usize + 123);
        let mut one = Ledger::new(BLOCK);
        one.record(0, &bytes).unwrap();
        let mut many = Ledger::new(BLOCK);
        let mut at = 0;
        for piece in bytes.chunks(7777) {
            many.record(at, piece).unwrap();
            at += piece.len() as u64;
        }
        assert_eq!(one.hashes(), many.hashes());
        assert_eq!(one.hashes().len(), 3);
        assert_eq!(one.digest(), <[u8; 32]>::from(Sha256::digest(&bytes)));
        assert_eq!(one.ranges(), vec![[0, bytes.len() as u64]]);
    }

    #[test]
    fn blocks_may_arrive_out_of_order() {
        let bytes = data(3 * BLOCK as usize + 99);
        let b = BLOCK as usize;
        let mut sequential = Ledger::new(BLOCK);
        sequential.record(0, &bytes).unwrap();

        let mut ledger = Ledger::new(BLOCK);
        ledger.record(2 * BLOCK, &bytes[2 * b..]).unwrap();
        assert_eq!(ledger.ranges(), vec![[2 * BLOCK, bytes.len() as u64]]);
        assert_eq!(ledger.hashes()[..2], [None, None]);
        ledger.record(0, &bytes[..b / 2]).unwrap();
        ledger.record(BLOCK, &bytes[b..2 * b]).unwrap();
        assert_eq!(ledger.ranges().len(), 2);
        ledger.record(BLOCK / 2, &bytes[b / 2..b]).unwrap();
        assert_eq!(ledger.ranges(), vec![[0, bytes.len() as u64]]);
        assert_eq!(ledger.hashes(), sequential.hashes());
        assert_eq!(ledger.bytes(), bytes.len() as u64);

        // The whole-file hash covers only what arrived in order until it catches up.
        assert_ne!(ledger.digest(), sequential.digest());
        let dir = tempfile::tempdir().unwrap();
        ledger.catch_up(&file_with(&dir, &bytes)).unwrap();
        assert_eq!(ledger.digest(), sequential.digest());
    }

    #[test]
    fn writes_must_continue_their_block() {
        let mut ledger = Ledger::new(BLOCK);
        ledger.record(0, b"abc").unwrap();
        assert!(ledger.record(2, b"x").is_err(), "overlap");
        assert!(ledger.record(10, b"x").is_err(), "gap inside a block");
        assert!(ledger.record(BLOCK + 1, b"x").is_err());
        assert!(
            ledger.record(BLOCK - 1, &[0; 2]).is_err(),
            "a piece that fails leaves nothing recorded"
        );
        assert_eq!(ledger.bytes(), 3);
        ledger.record(3, b"d").unwrap();
        assert_eq!(ledger.prefix(), 4);
    }

    #[test]
    fn recovers_the_verified_prefix_of_each_range() {
        let bytes = data(8 * BLOCK as usize);
        let b = BLOCK as usize;
        let mut ledger = Ledger::new(BLOCK);
        ledger.record(0, &bytes[..3 * b + 500]).unwrap();
        ledger.record(5 * BLOCK, &bytes[5 * b..7 * b + 9]).unwrap();
        let state = state_for(&ledger, 8 * BLOCK);
        assert_eq!(state.ranges.len(), 2);

        let dir = tempfile::tempdir().unwrap();
        let file = file_with(&dir, &bytes);
        let full = Ledger::recover(&file, &state).unwrap();
        assert_eq!(full.ranges(), ledger.ranges());
        assert_eq!(full.hashes(), ledger.hashes());
        assert_eq!(full.digest(), ledger.digest());

        // Continuing after recovery gives the same result as an uninterrupted run.
        let mut resumed = full.clone();
        resumed.record(3 * BLOCK + 500, b"tail").unwrap();
        let mut straight = ledger.clone();
        straight.record(3 * BLOCK + 500, b"tail").unwrap();
        assert_eq!(resumed.hashes(), straight.hashes());

        // Damage in block 1 keeps only block 0 of the first range; damage in block 6 keeps
        // block 5 of the second.
        file.write_all_at(&[bytes[b + 10] ^ 0xff], BLOCK + 10)
            .unwrap();
        file.write_all_at(&[bytes[6 * b] ^ 0xff], 6 * BLOCK)
            .unwrap();
        let damaged = Ledger::recover(&file, &state).unwrap();
        assert_eq!(damaged.ranges(), vec![[0, BLOCK], [5 * BLOCK, 6 * BLOCK]]);

        // A file shorter than the claims keeps whole blocks that are present.
        file.write_all_at(&[bytes[b + 10]], BLOCK + 10).unwrap();
        file.set_len(2 * BLOCK + 3).unwrap();
        assert_eq!(
            Ledger::recover(&file, &state).unwrap().ranges(),
            vec![[0, 2 * BLOCK]]
        );
    }
}

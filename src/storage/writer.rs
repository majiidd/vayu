//! Bounded, off-runtime writer for the partial file, and the checkpoint protocol.
//!
//! Disk writes, hashing, and checkpoints run on a dedicated thread so they never occupy
//! asynchronous networking workers. The command channel is bounded and shared by all
//! connections: a slow disk applies backpressure to the transfer instead of growing an in-memory
//! queue. Writes are positional; consecutive pieces of one stream are coalesced into runs of up
//! to [`RUN_SIZE`] before they reach the file.
//!
//! A checkpoint flushes and synchronizes the part file first, and only then publishes a new
//! state generation that claims those bytes. A crash between the two steps leaves the previous
//! generation, which claims less. The thread checkpoints after each interval in which data
//! arrived, and on request.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use super::ledger::Ledger;
use crate::fault;
use crate::metrics::Durable;
use crate::state::{self, MAX_BLOCKS, State};
use crate::transport::http::Validators;

/// Chunks queued ahead of the disk. Network chunks are typically 8–64 KiB.
const QUEUE_DEPTH: usize = 64;
/// Largest coalesced write.
const RUN_SIZE: usize = 256 << 10;
/// Buffered bytes across all runs before they are written out.
const BUFFER_SIZE: usize = 1 << 20;
/// Runs kept apart before they are written out; one per active stream is typical.
const MAX_RUNS: usize = 16;

enum Command {
    Data {
        offset: u64,
        data: Bytes,
    },
    Checkpoint(oneshot::Sender<()>),
    Reset(oneshot::Sender<()>),
    Snapshot {
        reply: oneshot::Sender<Ledger>,
    },
    Rebind {
        validators: Validators,
        reply: oneshot::Sender<()>,
    },
    Finish {
        complete: bool,
        reply: oneshot::Sender<Finished>,
    },
}

/// Where and how often checkpoints are published.
pub struct Checkpoints {
    pub state: State,
    pub path: PathBuf,
    pub tmp: PathBuf,
    pub interval: Duration,
    pub durable: Arc<Durable>,
}

/// The durable result of a write sequence.
pub struct Finished {
    pub file: File,
    /// Length of the contiguous data from offset zero.
    pub bytes: u64,
    /// SHA-256 of those bytes.
    pub sha256: [u8; 32],
}

pub struct PartWriter {
    handle: WriteHandle,
    thread: Option<JoinHandle<io::Result<()>>>,
}

/// Queues data for the writer; one per connection.
#[derive(Clone)]
pub struct WriteHandle {
    tx: mpsc::Sender<Command>,
}

/// The writer thread stopped; [`PartWriter::error`] says why.
#[derive(Debug)]
pub struct WriterGone;

impl WriteHandle {
    /// Queue bytes for `offset`; waits when the disk falls behind.
    pub async fn write(&self, offset: u64, data: Bytes) -> Result<(), WriterGone> {
        self.tx
            .send(Command::Data { offset, data })
            .await
            .map_err(|_| WriterGone)
    }

    /// The queue is nearly full: the disk is not keeping up with the network.
    pub fn congested(&self) -> bool {
        self.tx.capacity() < QUEUE_DEPTH / 4
    }
}

impl PartWriter {
    /// Take ownership of an open, locked part file whose saved data `ledger` describes. The
    /// first checkpoint is published before any new data is written.
    pub fn spawn(file: File, ledger: Ledger, checkpoints: Checkpoints) -> io::Result<PartWriter> {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let thread = std::thread::Builder::new()
            .name("vayu-writer".into())
            .spawn(move || run(file, ledger, checkpoints, rx))?;
        Ok(PartWriter {
            handle: WriteHandle { tx },
            thread: Some(thread),
        })
    }

    /// A handle for another connection's writes.
    pub fn handle(&self) -> WriteHandle {
        self.handle.clone()
    }

    /// Queue bytes for `offset`; waits when the disk falls behind.
    pub async fn write(&mut self, offset: u64, data: Bytes) -> io::Result<()> {
        if self.handle.write(offset, data).await.is_err() {
            return Err(self.error().await);
        }
        Ok(())
    }

    /// Make everything written so far durable and record it in the state file.
    pub async fn checkpoint(&mut self) -> io::Result<()> {
        let (reply, wait) = oneshot::channel();
        if self
            .handle
            .tx
            .send(Command::Checkpoint(reply))
            .await
            .is_err()
            || wait.await.is_err()
        {
            return Err(self.error().await);
        }
        Ok(())
    }

    /// Discard everything written so far. The state stops claiming the data before the file is
    /// truncated.
    pub async fn reset(&mut self) -> io::Result<()> {
        let (reply, wait) = oneshot::channel();
        if self.handle.tx.send(Command::Reset(reply)).await.is_err() || wait.await.is_err() {
            return Err(self.error().await);
        }
        Ok(())
    }

    /// Snapshot the ledger after all earlier queued writes. Callers stop other producers first.
    pub async fn snapshot(&mut self) -> io::Result<Ledger> {
        let (reply, wait) = oneshot::channel();
        let command = Command::Snapshot { reply };
        if self.handle.tx.send(command).await.is_err() {
            return Err(self.error().await);
        }
        match wait.await {
            Ok(samples) => Ok(samples),
            Err(_) => Err(self.error().await),
        }
    }

    /// Record that the saved data belongs to the representation `validators` names, and
    /// publish it.
    pub async fn rebind(&mut self, validators: Validators) -> io::Result<()> {
        let (reply, wait) = oneshot::channel();
        let command = Command::Rebind { validators, reply };
        if self.handle.tx.send(command).await.is_err() || wait.await.is_err() {
            return Err(self.error().await);
        }
        Ok(())
    }

    /// Flush, synchronize, and checkpoint all data, returning the file (still locked) and the
    /// digest of its contiguous data. `complete` records the final size in the state, so a run
    /// interrupted during finalization can finish without downloading again.
    pub async fn finish(mut self, complete: bool) -> io::Result<Finished> {
        let (reply, wait) = oneshot::channel();
        if self
            .handle
            .tx
            .send(Command::Finish { complete, reply })
            .await
            .is_err()
        {
            return Err(self.error().await);
        }
        match wait.await {
            Ok(done) => Ok(done),
            Err(_) => Err(self.error().await),
        }
    }

    /// Collect the writer thread's error after it stopped accepting commands.
    pub async fn error(&mut self) -> io::Error {
        let Some(thread) = self.thread.take() else {
            return io::Error::other("writer already stopped");
        };
        match tokio::task::spawn_blocking(move || thread.join()).await {
            Ok(Ok(Err(err))) => err,
            Ok(Ok(Ok(()))) => io::Error::other("writer stopped unexpectedly"),
            _ => io::Error::other("writer thread panicked"),
        }
    }
}

/// Contiguous bytes waiting to be written at `offset`.
struct Run {
    offset: u64,
    buf: Vec<u8>,
}

struct Worker {
    file: File,
    runs: Vec<Run>,
    buffered: usize,
    ledger: Ledger,
    cp: Checkpoints,
    /// Bytes claimed by the latest published generation.
    saved: Option<u64>,
    last: Instant,
}

impl Worker {
    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.ledger.record(offset, data)?;
        let len = data.len();
        match self
            .runs
            .iter_mut()
            .find(|r| r.offset + r.buf.len() as u64 == offset && r.buf.len() + len <= RUN_SIZE)
        {
            Some(run) => run.buf.extend_from_slice(data),
            None => self.runs.push(Run {
                offset,
                buf: data.to_vec(),
            }),
        }
        self.buffered += len;
        if self.buffered >= BUFFER_SIZE || self.runs.len() > MAX_RUNS {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        for run in self.runs.drain(..) {
            self.file.write_all_at(&run.buf, run.offset)?;
        }
        self.buffered = 0;
        Ok(())
    }

    fn checkpoint(&mut self) -> io::Result<()> {
        self.flush()?;
        self.file.sync_data()?;
        fault::point("checkpoint-synced");
        self.ledger.catch_up(&self.file)?;
        let bytes = self.ledger.bytes();
        let hashes = self.ledger.hashes();
        // Past the block limit (only possible for >1 TiB of unknown length) the last valid
        // generation stays in place.
        if hashes.len() as u64 <= MAX_BLOCKS {
            let state = &mut self.cp.state;
            state.generation += 1;
            state.ranges = self.ledger.ranges();
            state.blocks = hashes;
            state::publish(state, &self.cp.path, &self.cp.tmp)?;
            fault::point("checkpoint-published");
            self.cp.durable.record(bytes);
            self.saved = Some(bytes);
        }
        self.last = Instant::now();
        Ok(())
    }

    fn checkpoint_if_new(&mut self) -> io::Result<()> {
        if self.saved != Some(self.ledger.bytes()) {
            self.checkpoint()?;
        }
        Ok(())
    }
}

fn run(
    file: File,
    ledger: Ledger,
    cp: Checkpoints,
    mut rx: mpsc::Receiver<Command>,
) -> io::Result<()> {
    let mut w = Worker {
        file,
        runs: Vec::new(),
        buffered: 0,
        ledger,
        cp,
        saved: None,
        last: Instant::now(),
    };
    w.checkpoint()?;
    while let Some(command) = rx.blocking_recv() {
        match command {
            Command::Data { offset, data } => {
                w.write(offset, &data)?;
                if w.last.elapsed() >= w.cp.interval {
                    w.checkpoint()?;
                }
            }
            Command::Checkpoint(reply) => {
                w.checkpoint_if_new()?;
                let _ = reply.send(());
            }
            Command::Reset(reply) => {
                w.runs.clear();
                w.buffered = 0;
                w.ledger = Ledger::new(w.ledger.block_size());
                w.checkpoint()?;
                w.file.set_len(0)?;
                let _ = reply.send(());
            }
            Command::Snapshot { reply } => {
                let _ = reply.send(w.ledger.clone());
            }
            Command::Rebind { validators, reply } => {
                w.cp.state.validators = validators;
                w.checkpoint()?;
                let _ = reply.send(());
            }
            Command::Finish { complete, reply } => {
                if complete && w.cp.state.total.is_none() {
                    w.cp.state.total = Some(w.ledger.bytes());
                    w.saved = None;
                }
                w.checkpoint_if_new()?;
                w.ledger.catch_up(&w.file)?;
                let _ = reply.send(Finished {
                    bytes: w.ledger.prefix(),
                    sha256: w.ledger.digest(),
                    file: w.file,
                });
                return Ok(());
            }
        }
    }
    // The transfer side went away without finishing; persist what was written.
    w.checkpoint_if_new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn setup(dir: &tempfile::TempDir, total: Option<u64>) -> (PartWriter, Arc<Durable>) {
        let path = dir.path().join("x.part");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let url = url::Url::parse("https://example.com/x").unwrap();
        let state = State::new(&url, "x".into(), total, Validators::default(), None);
        let durable = Arc::new(Durable::default());
        let cp = Checkpoints {
            path: dir.path().join("x.vayu"),
            tmp: dir.path().join("x.vayu.tmp"),
            interval: Duration::from_secs(3600),
            durable: durable.clone(),
            state,
        };
        let block_size = cp.state.block_size;
        let writer = PartWriter::spawn(file, Ledger::new(block_size), cp).unwrap();
        (writer, durable)
    }

    #[tokio::test]
    async fn snapshot_includes_every_earlier_queued_partial_write() {
        let dir = tempfile::tempdir().unwrap();
        let (mut writer, _) = setup(&dir, Some(4 << 20));
        let handle = writer.handle();
        handle
            .write(0, Bytes::from(vec![7; (1 << 20) + 100]))
            .await
            .unwrap();
        handle
            .write(3 << 20, Bytes::from_static(b"tail"))
            .await
            .unwrap();
        let snapshot = writer.snapshot().await.unwrap();
        assert_eq!(
            snapshot.ranges(),
            vec![[0, (1 << 20) + 100], [3 << 20, (3 << 20) + 4]]
        );
        assert_eq!(snapshot.verification_blocks().len(), 3);
        drop(handle);
        writer.finish(false).await.unwrap();
    }

    #[tokio::test]
    async fn writes_resets_hashes_and_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let (mut writer, durable) = setup(&dir, None);
        writer.write(0, Bytes::from_static(b"stale")).await.unwrap();
        writer.checkpoint().await.unwrap();
        assert_eq!(state::load(&dir.path().join("x.vayu")).unwrap().prefix(), 5);
        assert_eq!(durable.bytes(), 5);

        writer.reset().await.unwrap();
        let after_reset = state::load(&dir.path().join("x.vayu")).unwrap();
        assert_eq!(after_reset.prefix(), 0);
        writer
            .write(0, Bytes::from_static(b"hello "))
            .await
            .unwrap();
        writer.write(6, Bytes::from_static(b"world")).await.unwrap();
        let done = writer.finish(true).await.unwrap();
        assert_eq!(done.bytes, 11);
        let expected: [u8; 32] = Sha256::digest(b"hello world").into();
        assert_eq!(done.sha256, expected);
        assert_eq!(
            std::fs::read(dir.path().join("x.part")).unwrap(),
            b"hello world"
        );

        let last = state::load(&dir.path().join("x.vayu")).unwrap();
        assert_eq!((last.prefix(), last.total), (11, Some(11)));
        assert!(last.generation > after_reset.generation);
    }

    #[tokio::test]
    async fn interleaved_positional_writes_assemble_the_file() {
        const B: usize = 1 << 20;
        let body: Vec<u8> = (0..3 * B + 777).map(|i| (i % 253) as u8).collect();
        let dir = tempfile::tempdir().unwrap();
        let (mut writer, durable) = setup(&dir, Some(body.len() as u64));
        let other = writer.handle();
        // Two streams: one from the start, one from block 2, alternating chunks.
        let (mut a, mut b) = (0usize, 2 * B);
        while a < 2 * B || b < body.len() {
            if a < 2 * B {
                let n = 40_000.min(2 * B - a);
                let chunk = Bytes::copy_from_slice(&body[a..a + n]);
                writer.write(a as u64, chunk).await.unwrap();
                a += n;
            }
            if b < body.len() {
                let n = 33_333.min(body.len() - b);
                let chunk = Bytes::copy_from_slice(&body[b..b + n]);
                other.write(b as u64, chunk).await.unwrap();
                b += n;
            }
            if a == B {
                writer.checkpoint().await.unwrap();
                let state = state::load(&dir.path().join("x.vayu")).unwrap();
                assert_eq!(state.ranges.len(), 2, "{:?}", state.ranges);
                assert_eq!(
                    durable.bytes(),
                    state.ranges.iter().map(|r| r[1] - r[0]).sum::<u64>()
                );
            }
        }
        let done = writer.finish(true).await.unwrap();
        assert_eq!(done.bytes, body.len() as u64);
        assert_eq!(done.sha256, <[u8; 32]>::from(Sha256::digest(&body)));
        assert_eq!(std::fs::read(dir.path().join("x.part")).unwrap(), body);
        let state = state::load(&dir.path().join("x.vayu")).unwrap();
        assert_eq!(state.ranges, vec![[0, body.len() as u64]]);
    }

    #[tokio::test]
    async fn an_out_of_order_write_stops_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let (mut writer, _) = setup(&dir, None);
        writer.write(0, Bytes::from_static(b"abc")).await.unwrap();
        writer.write(1, Bytes::from_static(b"x")).await.unwrap();
        let err = writer.checkpoint().await.unwrap_err();
        assert!(err.to_string().contains("does not continue"), "{err}");
    }
}

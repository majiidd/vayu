//! Finding saved progress, deciding whether it can be continued safely, and completing a
//! finalization that an earlier run did not finish.
//!
//! Rules:
//! * The partial file is locked before its state is acted on, so a running download is never
//!   disturbed.
//! * A state file is trusted only after its digest and bounds check out, and saved bytes only
//!   after their block hashes match. Claims beyond the end of the partial file count as missing.
//! * A rerun matches saved progress automatically only when the request identity is the same.
//!   `--resume-from` selects a state file explicitly, for example with a refreshed signed URL.
//!   Either way the source is revalidated before more bytes are appended.
//! * Continuing needs a strong validator or an expected checksum that gates publication.
//!   Otherwise the data is kept and the user is told how to proceed.
//! * Nothing here deletes saved data, except the recovery files of a download whose final file
//!   is already in place and verified against the saved block hashes.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::Settings;
use crate::error::{Category, Failure, describe_io};
use crate::integrity::{self, Checksum};
use crate::metrics::{Phase, Reporter};
use crate::presentation::Completion;
use crate::state::{self, LoadError, Source, State};
use crate::storage::{self, Destination, FALLBACK_NAME, Ledger, STATE_SUFFIX};

pub enum Found {
    /// No saved progress for this destination.
    Nothing,
    /// Saved state without usable data; its files may be taken over. `discarded` bytes on disk
    /// could not be verified.
    Empty { dest: Destination, discarded: u64 },
    /// Verified saved progress, ready to continue.
    Resume(Box<Recovered>),
    /// An earlier run published the file but stopped before removing its recovery files.
    Published(Completion),
    /// Saved progress for a different URL occupies this destination.
    Foreign(Failure),
}

pub struct Recovered {
    pub dest: Destination,
    /// The locked partial file, truncated after its last verified byte.
    pub file: File,
    pub ledger: Ledger,
    /// The saved state, updated to the current request.
    pub state: State,
    /// The expected checksum: from the command line, or saved by the earlier run.
    pub checksum: Option<Checksum>,
    /// Saved bytes that failed verification, and bytes on disk beyond the saved ranges; they
    /// will be downloaded again.
    pub discarded: u64,
}

/// Look for saved progress before sending any request: the state file named by
/// `--resume-from`, or the one next to the destination known from `-o` or the URL.
pub fn locate(settings: &Settings, reporter: &Reporter) -> Result<Found, Failure> {
    if let Some(from) = &settings.resume_from {
        let dest = from_state(from)?;
        if let Some(output) = &settings.output
            && Destination::resolve(Some(output), &dest.file_name()).path != dest.path
        {
            return Err(Failure::usage(format!(
                "-o {} does not match the saved download {}",
                output.display(),
                dest.path.display()
            ))
            .with_hint("omit -o; the download continues where it was saved"));
        }
        return match examine(&dest, settings, true, reporter)? {
            Found::Nothing => Err(missing_state(&dest.state)),
            found => Ok(found),
        };
    }
    if settings.restart {
        return Ok(Found::Nothing);
    }
    let (dest, explicit) = match Destination::explicit(settings.output.as_deref()) {
        Some(dest) => (dest, true),
        None => {
            let name = storage::filename_from_url(&settings.url)
                .unwrap_or_else(|| FALLBACK_NAME.to_string());
            (
                Destination::resolve(settings.output.as_deref(), &name),
                false,
            )
        }
    };
    match examine(&dest, settings, false, reporter)? {
        // The server may still name a different file; decide once the destination is final.
        Found::Foreign(_) if !explicit => Ok(Found::Nothing),
        found => Ok(found),
    }
}

/// Inspect the recovery files of one destination. `by_user` means the user selected the state
/// file explicitly, which waives the request-identity match.
pub fn examine(
    dest: &Destination,
    settings: &Settings,
    by_user: bool,
    reporter: &Reporter,
) -> Result<Found, Failure> {
    dest.check_parent()?;
    if fs::symlink_metadata(&dest.state).is_err() {
        return Ok(Found::Nothing);
    }
    let part = match fs::symlink_metadata(&dest.part) {
        Ok(meta) if !meta.is_file() => {
            return Err(Failure::conflict(format!(
                "{} exists and is not a regular file",
                dest.part.display()
            )));
        }
        Ok(_) => {
            let file = storage::safety::open(&dest.part, true)
                .map_err(|e| io_failure("open", &dest.part, &e))?;
            storage::lock(&file, &dest.part)?;
            Some(file)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => return Err(io_failure("open", &dest.part, &err)),
    };

    let state = match state::load(&dest.state) {
        Ok(state) => state,
        Err(LoadError::Io(err)) if err.kind() == ErrorKind::NotFound => return Ok(Found::Nothing),
        Err(LoadError::Io(err)) => return Err(io_failure("read", &dest.state, &err)),
        Err(LoadError::Invalid(reason)) => {
            return Err(Failure::conflict(format!(
                "cannot use the saved progress in {}: {reason}",
                dest.state.display()
            ))
            .with_hint(format!(
                "{} was left untouched; run again with --restart to start over",
                dest.part.display()
            )));
        }
    };
    if state.file_name != dest.file_name() {
        return Err(Failure::conflict(format!(
            "{} describes a download of `{}`, not `{}`",
            dest.state.display(),
            state.file_name,
            dest.file_name()
        ))
        .with_hint("rename it back, or run again with --restart to start over"));
    }
    if !by_user && state.source.request != state::request_identity(&settings.url) {
        return Ok(Found::Foreign(
            Failure::conflict(format!(
                "{} holds saved progress for a different URL ({})",
                dest.state.display(),
                state.source.url
            ))
            .with_hint(format!(
                "to continue it with this URL, add --resume-from {}; to start over, use --restart",
                dest.state.display()
            )),
        ));
    }
    let checksum = settings.checksum.clone().or_else(|| state.checksum());

    if fs::symlink_metadata(&dest.path).is_ok() {
        // The lock on the partial file, if any, is held until cleanup is done.
        let done = finish_published(dest, &state, checksum.as_ref(), reporter, part.as_ref())?;
        drop(part);
        return Ok(Found::Published(done));
    }
    let Some(file) = part else {
        return Ok(Found::Empty {
            dest: dest.clone(),
            discarded: 0,
        });
    };
    storage::safety::single_link(&file).map_err(|e| io_failure("use", &dest.part, &e))?;

    reporter.phase(Phase::Recovering);
    let ledger = Ledger::recover(&file, &state).map_err(|e| io_failure("read", &dest.part, &e))?;
    let len = file
        .metadata()
        .map_err(|e| io_failure("read", &dest.part, &e))?
        .len();
    if ledger.bytes() == 0 {
        return Ok(Found::Empty {
            dest: dest.clone(),
            discarded: len,
        });
    }
    let complete = state.total == Some(ledger.prefix());
    if !complete && state.validators.if_range().is_none() && checksum.is_none() {
        return Err(Failure::conflict(format!(
            "the saved progress in {} cannot be matched to the file on the server",
            dest.part.display()
        ))
        .with_hint(
            "the server sent no strong validator (ETag), so new data could belong to a \
             different version; supply the expected --checksum sha256:HEX to continue, or use \
             --restart",
        ));
    }

    let discarded = discarded(&state, &ledger, len);
    let end = ledger.ranges().last().map_or(0, |r| r[1]);
    file.set_len(end)
        .map_err(|e| io_failure("write", &dest.part, &e))?;
    let mut state = state;
    state.source = Source::of(&settings.url);
    state.checksum = checksum.as_ref().map(ToString::to_string);
    Ok(Found::Resume(Box::new(Recovered {
        dest: dest.clone(),
        file,
        ledger,
        state,
        checksum,
        discarded,
    })))
}

/// Bytes that will be downloaded again although they are on disk: claimed bytes that failed
/// verification, and data written after the last checkpoint.
fn discarded(state: &State, ledger: &Ledger, len: u64) -> u64 {
    let claimed: u64 = state
        .ranges
        .iter()
        .map(|&[start, end]| end.min(len).saturating_sub(start))
        .sum();
    let claimed_end = state.ranges.last().map_or(0, |r| r[1]);
    claimed.saturating_sub(ledger.bytes()) + len.saturating_sub(claimed_end)
}

/// The destination exists. If it is this download's complete, verified result, remove the
/// leftover recovery files; otherwise it belongs to someone else and stays untouched.
fn finish_published(
    dest: &Destination,
    state: &State,
    checksum: Option<&Checksum>,
    reporter: &Reporter,
    part: Option<&File>,
) -> Result<Completion, Failure> {
    let exists = || {
        Failure::conflict(format!("{} already exists", dest.path.display()))
            .with_hint("choose another destination with -o, or move the existing file")
    };
    let Some(total) = state.total.filter(|&t| t == state.prefix()) else {
        return Err(exists());
    };
    let file = storage::safety::open(&dest.path, false).map_err(|_| exists())?;
    if let Some(part) = part {
        let original = part.metadata().map_err(|_| exists())?;
        let published = file.metadata().map_err(|_| exists())?;
        if !storage::safety::same_file(&original, &published) {
            return Err(exists());
        }
    }
    if file.metadata().map(|m| m.len()).ok() != Some(total) {
        return Err(exists());
    }
    reporter.phase(Phase::Recovering);
    let ledger = Ledger::recover(&file, state).map_err(|e| io_failure("read", &dest.path, &e))?;
    if ledger.prefix() != total {
        return Err(exists());
    }
    let sha256 = ledger.digest();
    if let Some(expected) = checksum
        && !expected.matches(&sha256)
    {
        return Err(Failure::new(
            Category::Integrity,
            format!(
                "{} does not match the expected SHA-256: expected {}, got {}",
                dest.path.display(),
                expected.hex(),
                integrity::hex(&sha256)
            ),
        ));
    }
    if fs::symlink_metadata(&dest.part).is_ok() {
        let _ = fs::remove_file(&dest.part);
    }
    dest.remove_state();
    Ok(Completion {
        path: dest.path.clone(),
        bytes: total,
        elapsed: Duration::ZERO,
        sha256,
        verified: checksum.is_some(),
    })
}

/// Accept `NAME.vayu`, or `NAME` / `NAME.part` next to it.
fn from_state(path: &Path) -> Result<Destination, Failure> {
    let raw = path.as_os_str().to_string_lossy();
    let state_path = if raw.ends_with(STATE_SUFFIX) {
        path.to_path_buf()
    } else {
        let base = raw.strip_suffix(".part").unwrap_or(&raw);
        PathBuf::from(format!("{base}{STATE_SUFFIX}"))
    };
    if !state_path.is_file() {
        return Err(missing_state(&state_path));
    }
    Destination::of_state(&state_path).ok_or_else(|| missing_state(&state_path))
}

fn missing_state(path: &Path) -> Failure {
    Failure::usage(format!("no saved progress at {}", path.display()))
        .with_hint("pass the NAME.vayu file that sits next to the partial download")
}

fn io_failure(action: &str, path: &Path, err: &std::io::Error) -> Failure {
    Failure::storage(format!(
        "cannot {action} {}: {}",
        path.display(),
        describe_io(err)
    ))
}

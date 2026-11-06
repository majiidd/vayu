//! Destination selection, partial-file ownership, and safe publication.
//!
//! Data is written to `NAME.part` next to the destination, with its saved progress in
//! `NAME.vayu`. The file is published with a no-replace link (or rename) only after it is
//! complete, synchronized, and verified. An existing destination is never overwritten.

pub mod ledger;
pub mod safety;
pub mod writer;

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;
use url::Url;

use crate::error::{Failure, describe_io};
use crate::fault;
pub use ledger::Ledger;
pub use writer::{Checkpoints, Finished, PartWriter, WriteHandle};

const MAX_NAME_BYTES: usize = 200;
pub const FALLBACK_NAME: &str = "download";
pub const STATE_SUFFIX: &str = ".vayu";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    pub path: PathBuf,
    pub part: PathBuf,
    /// Saved progress, `NAME.vayu`.
    pub state: PathBuf,
    /// Staging file for atomic state replacement.
    pub state_tmp: PathBuf,
}

impl Destination {
    /// Choose the destination: `-o PATH`, a name inside `-o DIR/`, or a name in the current
    /// directory. Returns `None` when a server-suggested name is still needed.
    pub fn explicit(output: Option<&Path>) -> Option<Destination> {
        let output = output?;
        (!is_directory_target(output)).then(|| Destination::at(output.to_path_buf()))
    }

    pub fn resolve(output: Option<&Path>, suggested: &str) -> Destination {
        let path = match output {
            Some(dir) if is_directory_target(dir) => dir.join(suggested),
            Some(file) => file.to_path_buf(),
            None => PathBuf::from(suggested),
        };
        Destination::at(path)
    }

    /// The destination a state file belongs to: `X.vayu` saves progress for `X`. Paths are
    /// derived from the state file's location, so a moved directory still resumes.
    pub fn of_state(state: &Path) -> Option<Destination> {
        let name = state.file_name()?.to_str()?;
        let stem = name.strip_suffix(STATE_SUFFIX).filter(|s| !s.is_empty())?;
        Some(Destination::at(state.with_file_name(stem)))
    }

    fn at(path: PathBuf) -> Destination {
        let path = std::path::absolute(&path).unwrap_or(path);
        // Keep the resolved namespace, rather than looking up a mutable symlink alias later.
        let path = match (
            path.parent().and_then(|p| p.canonicalize().ok()),
            path.file_name(),
        ) {
            (Some(parent), Some(name)) => parent.join(name),
            _ => path,
        };
        let with = |suffix: &str| {
            let mut name = OsString::from(path.as_os_str());
            name.push(suffix);
            PathBuf::from(name)
        };
        Destination {
            part: with(".part"),
            state: with(STATE_SUFFIX),
            state_tmp: with(".vayu.tmp"),
            path,
        }
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Fail early, before any network traffic, when the destination is already taken.
    pub fn precheck(&self, restart: bool) -> Result<(), Failure> {
        self.ensure_free()?;
        if !restart && fs::symlink_metadata(&self.part).is_ok() {
            return Err(self.incomplete_conflict());
        }
        Ok(())
    }

    /// Create (or with `take_over`, take over and empty) the locked part file.
    pub fn open_part(&self, take_over: bool) -> Result<File, Failure> {
        self.ensure_free()?;
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.part)
        {
            Ok(file) => {
                safety::single_link(&file).map_err(|e| self.io_failure(&e))?;
                lock(&file, &self.part)?;
                Ok(file)
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&self.part).map_err(|e| self.io_failure(&e))?;
                if !meta.is_file() {
                    return Err(Failure::conflict(format!(
                        "{} exists and is not a regular file",
                        self.part.display()
                    )));
                }
                let file = safety::open(&self.part, true).map_err(|e| self.io_failure(&e))?;
                safety::single_link(&file).map_err(|e| self.io_failure(&e))?;
                lock(&file, &self.part)?;
                if !take_over {
                    return Err(self.incomplete_conflict());
                }
                file.set_len(0).map_err(|e| self.io_failure(&e))?;
                Ok(file)
            }
            Err(err) => Err(self.io_failure(&err)),
        }
    }

    /// Remove the part file and its state if nothing useful was written.
    pub fn discard_if_empty(&self) {
        if fs::metadata(&self.part).is_ok_and(|m| m.is_file() && m.len() == 0) {
            let _ = fs::remove_file(&self.part);
            self.remove_state();
        }
    }

    /// Remove the state file once it no longer describes anything worth recovering.
    pub fn remove_state(&self) {
        let _ = fs::remove_file(&self.state_tmp);
        let _ = fs::remove_file(&self.state);
        if let Some(parent) = self.path.parent() {
            sync_dir(parent);
        }
    }

    /// Publish a complete, synchronized part file under the final name without replacing
    /// anything already there, then remove the recovery files.
    ///
    /// Every intermediate state is recoverable: until the link exists, the state file says the
    /// data is complete; after it, the final file can be checked against the state's block
    /// hashes and the leftovers removed.
    pub fn publish(&self, finished: Finished) -> Result<(), Failure> {
        self.check_parent()?;
        let actual = fs::symlink_metadata(&self.part).map_err(|e| self.io_failure(&e))?;
        let verified = safety::validate(&finished.file).map_err(|e| self.io_failure(&e))?;
        if !actual.is_file() || !safety::same_file(&actual, &verified) || verified.nlink() != 1 {
            return Err(Failure::conflict(
                "the partial file was replaced before publication",
            ));
        }
        fault::point("finalize-before-link");
        match fs::hard_link(&self.part, &self.path) {
            Ok(()) => {
                if let Some(parent) = self.path.parent() {
                    sync_dir(parent);
                }
                fault::point("finalize-linked");
                let _ = fs::remove_file(&self.part);
                fault::point("finalize-part-removed");
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                return Err(self.appeared_conflict());
            }
            // Some file systems (FAT, some network mounts) lack hard links.
            Err(_) => {
                safety::rename_noreplace(&self.part, &self.path).map_err(|err| {
                    if err.kind() == ErrorKind::AlreadyExists {
                        return self.appeared_conflict();
                    }
                    Failure::storage(format!(
                        "cannot move {} into place: {}",
                        self.part.display(),
                        describe_io(&err)
                    ))
                })?;
            }
        }
        self.remove_state();
        drop(finished.file);
        Ok(())
    }

    fn ensure_free(&self) -> Result<(), Failure> {
        self.check_parent()?;
        if fs::symlink_metadata(&self.path).is_ok() {
            return Err(
                Failure::conflict(format!("{} already exists", self.path.display()))
                    .with_hint("choose another destination with -o, or move the existing file"),
            );
        }
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.is_dir()
        {
            return Err(Failure::storage(format!(
                "directory {} does not exist",
                parent.display()
            )));
        }
        Ok(())
    }

    pub fn check_parent(&self) -> Result<(), Failure> {
        safety::check_parent(&self.path).map_err(|err| {
            Failure::conflict(format!("cannot safely use {}: {err}", self.path.display()))
                .with_hint("choose an owner-controlled destination directory with -o")
        })
    }

    fn incomplete_conflict(&self) -> Failure {
        Failure::conflict(format!(
            "an incomplete download without saved progress exists at {}",
            self.part.display()
        ))
        .with_hint(format!(
            "it cannot be resumed without {}; run again with --restart to start over",
            self.state.display()
        ))
    }

    fn appeared_conflict(&self) -> Failure {
        Failure::conflict(format!(
            "{} appeared while downloading; it was not replaced",
            self.path.display()
        ))
        .with_hint(format!(
            "the downloaded data is kept in {}",
            self.part.display()
        ))
    }

    fn io_failure(&self, err: &std::io::Error) -> Failure {
        Failure::storage(format!(
            "cannot write {}: {}",
            self.part.display(),
            describe_io(err)
        ))
    }
}

fn is_directory_target(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().ends_with('/') || path.is_dir()
}

pub fn lock(file: &File, path: &Path) -> Result<(), Failure> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(Failure::conflict(format!(
            "another vayu process is downloading to {}",
            path.display()
        ))),
        Err(TryLockError::Error(err)) if err.kind() == ErrorKind::Unsupported => Ok(()),
        Err(TryLockError::Error(err)) => Err(Failure::storage(format!(
            "cannot lock {}: {}",
            path.display(),
            describe_io(&err)
        ))),
    }
}

/// Best-effort directory synchronization, so a rename or unlink survives a crash. Some
/// filesystems do not support it.
pub fn sync_dir(dir: &Path) {
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

/// Reduce a server- or URL-provided name to a safe single path component.
pub fn sanitize_filename(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !crate::presentation::text::is_invisible_control(*c))
        .collect();
    let cleaned = cleaned
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' ']);
    if cleaned.is_empty() {
        return None;
    }
    Some(limit_name(cleaned))
}

fn limit_name(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if name.len() - dot <= 16 => name.split_at(dot),
        _ => (name, ""),
    };
    let mut end = MAX_NAME_BYTES - ext.len();
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ext}", &stem[..end])
}

/// The last non-empty path segment of a URL, percent-decoded and sanitized.
pub fn filename_from_url(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    let decoded = percent_decode_str(segment).decode_utf8_lossy();
    sanitize_filename(&decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_filename("file.zip").as_deref(), Some("file.zip"));
        assert_eq!(
            sanitize_filename("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            sanitize_filename("..\\evil.exe").as_deref(),
            Some("evil.exe")
        );
        assert_eq!(sanitize_filename(".bashrc").as_deref(), Some("bashrc"));
        assert_eq!(
            sanitize_filename("a\u{1b}[31mb\u{202e}c.txt").as_deref(),
            Some("a[31mbc.txt")
        );
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("dir/"), None);
        let long = format!("{}.tar.gz", "x".repeat(300));
        let limited = sanitize_filename(&long).unwrap();
        assert!(limited.len() <= MAX_NAME_BYTES && limited.ends_with(".gz"));
    }

    #[test]
    fn names_from_urls() {
        let url = Url::parse("https://example.com/a/linux%20image.iso?x=1").unwrap();
        assert_eq!(filename_from_url(&url).as_deref(), Some("linux image.iso"));
        let url = Url::parse("https://example.com/files/").unwrap();
        assert_eq!(filename_from_url(&url).as_deref(), Some("files"));
        assert_eq!(
            filename_from_url(&Url::parse("https://example.com/").unwrap()),
            None
        );
    }

    #[test]
    fn part_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let dest = Destination::resolve(Some(dir.path()), "f.bin");
        assert_eq!(dest.path, dir.path().canonicalize().unwrap().join("f.bin"));
        assert_eq!(
            dest.part,
            dir.path().canonicalize().unwrap().join("f.bin.part")
        );
        dest.precheck(false).unwrap();

        let file = dest.open_part(false).unwrap();
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
        // A second claimant is refused while the first holds the lock, even with --restart.
        let err = dest.open_part(true).unwrap_err();
        assert!(
            err.message.contains("another vayu process"),
            "{}",
            err.message
        );
        drop(file);

        let err = dest.precheck(false).unwrap_err();
        assert!(err.message.contains("incomplete download"));
        dest.precheck(true).unwrap();
        let file = dest.open_part(true).unwrap();
        file.sync_all().unwrap();
        dest.publish(Finished {
            file,
            bytes: 0,
            sha256: [0; 32],
        })
        .unwrap();
        assert!(dest.path.exists() && !dest.part.exists());
        assert!(
            dest.precheck(true)
                .unwrap_err()
                .message
                .contains("already exists")
        );
    }

    #[test]
    fn publish_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let dest = Destination::resolve(Some(dir.path()), "f.bin");
        let file = dest.open_part(false).unwrap();
        fs::write(&dest.path, b"someone else's file").unwrap();
        let err = dest
            .publish(Finished {
                file,
                bytes: 0,
                sha256: [0; 32],
            })
            .unwrap_err();
        assert!(err.message.contains("appeared"));
        assert_eq!(fs::read(&dest.path).unwrap(), b"someone else's file");
        assert!(dest.part.exists());
    }

    #[test]
    fn publication_rejects_a_replaced_partial_path() {
        let dir = tempfile::tempdir().unwrap();
        let dest = Destination::resolve(Some(dir.path()), "f.bin");
        let file = dest.open_part(false).unwrap();
        fs::remove_file(&dest.part).unwrap();
        fs::write(&dest.part, b"unverified replacement").unwrap();
        assert!(
            dest.publish(Finished {
                file,
                bytes: 0,
                sha256: [0; 32]
            })
            .is_err()
        );
        assert!(!dest.path.exists());
    }

    #[test]
    fn restart_rejects_a_hard_link_to_another_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = Destination::resolve(Some(dir.path()), "f.bin");
        let outside = dir.path().join("unrelated");
        fs::write(&outside, b"keep this file").unwrap();
        fs::hard_link(&outside, &dest.part).unwrap();
        assert!(dest.open_part(true).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"keep this file");
    }
}

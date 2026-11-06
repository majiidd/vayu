//! Filesystem checks shared by creation, recovery, checkpoints, and publication.
//!
//! Pathname publication is safe only in a namespace another user cannot replace. Resolve
//! aliases once, check the ancestor chain, and validate opened objects before changing them.

use std::ffi::CString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

fn unsafe_file() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsafe download file or directory; choose a private destination directory",
    )
}

/// Resolve and check every directory that could let another user replace the output parent.
pub fn parent(path: &Path) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let canonical = parent.canonicalize()?;
    // SAFETY: geteuid only reads process credentials.
    let uid = unsafe { libc::geteuid() };
    for dir in canonical.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir()
            || (meta.uid() != uid && meta.uid() != 0)
            || (meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0)
        {
            return Err(unsafe_file());
        }
        check_acl(dir)?;
    }
    Ok(canonical)
}

pub fn check_parent(path: &Path) -> io::Result<()> {
    let canonical = parent(path)?;
    if path.parent() != Some(canonical.as_path()) {
        return Err(unsafe_file());
    }
    Ok(())
}

/// Open without following a final symlink or blocking on a planted FIFO. No truncation yet.
pub fn open(path: &Path, writable: bool) -> io::Result<File> {
    let path = parent(path)?.join(path.file_name().ok_or(io::ErrorKind::InvalidInput)?);
    let file = OpenOptions::new()
        .read(true)
        .write(writable)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    validate(&file)?;
    check_acl(&path)?;
    Ok(file)
}

pub fn validate(file: &File) -> io::Result<Metadata> {
    let meta = file.metadata()?;
    // SAFETY: geteuid only reads process credentials.
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        return Err(unsafe_file());
    }
    Ok(meta)
}

/// A file with other names may belong to another workflow; never truncate or write it.
pub fn single_link(file: &File) -> io::Result<()> {
    if validate(file)?.nlink() != 1 {
        return Err(unsafe_file());
    }
    Ok(())
}

pub fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Atomic no-replace fallback for filesystems that do not support hard links.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: the C strings remain live for the call; neither API replaces an existing target.
    #[cfg(target_vendor = "apple")]
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        ) as libc::c_int
    };
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    let result = {
        let _ = (from, to);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replace rename is unavailable",
        ));
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

// Linux POSIX ACL write grants are reflected in the group-class mode mask checked above.
#[cfg(not(target_vendor = "apple"))]
fn check_acl(_path: &Path) -> io::Result<()> {
    Ok(())
}

// Darwin extended ACL grants are independent of mode bits. Reject mutating allow entries;
// ordinary read grants and the common inherited deny-delete entry remain supported.
#[cfg(target_vendor = "apple")]
fn check_acl(path: &Path) -> io::Result<()> {
    use std::ffi::c_void;
    unsafe extern "C" {
        fn acl_get_file(path: *const libc::c_char, kind: libc::c_int) -> *mut c_void;
        fn acl_get_entry(
            acl: *mut c_void,
            entry_id: libc::c_int,
            entry: *mut *mut c_void,
        ) -> libc::c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut libc::c_int) -> libc::c_int;
        fn acl_get_permset_mask_np(entry: *mut c_void, mask: *mut u64) -> libc::c_int;
        fn acl_free(acl: *mut c_void) -> libc::c_int;
    }
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: ACL handles/entries belong to this call and are freed on every return path.
    unsafe {
        let acl = acl_get_file(path.as_ptr(), 0x100); // ACL_TYPE_EXTENDED
        if acl.is_null() {
            let err = io::Error::last_os_error();
            // Darwin reports ENOENT when the existing object has no extended ACL.
            if err.kind() == io::ErrorKind::NotFound {
                return Ok(());
            }
            return Err(err);
        }
        let result = (|| {
            let mut index = 0; // ACL_FIRST_ENTRY, then ACL_NEXT_ENTRY
            loop {
                let mut entry = std::ptr::null_mut();
                if acl_get_entry(acl, index, &mut entry) != 0 {
                    break;
                }
                index = -1;
                let mut tag = 0;
                let mut mask = 0;
                if acl_get_tag_type(entry, &mut tag) != 0
                    || acl_get_permset_mask_np(entry, &mut mask) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                // write/add, delete, append/add-directory, delete-child, write-security, chown
                if tag == 1
                    && mask & ((1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 12) | (1 << 13))
                        != 0
                {
                    return Err(unsafe_file());
                }
            }
            Ok(())
        })();
        acl_free(acl);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn unsafe_ancestors_and_writable_files_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        let child = shared.join("private");
        fs::create_dir(&shared).unwrap();
        fs::create_dir(&child).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(parent(&child.join("f")).is_err());
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(parent(&child.join("f")).is_ok());
        let file = child.join("f");
        fs::write(&file, b"untouched").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(open(&file, true).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"untouched");
    }

    #[test]
    fn open_refuses_symlinks_and_fifos_without_modifying_targets() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        fs::write(&target, b"untouched").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open(&link, true).is_err());
        let fifo = dir.path().join("fifo");
        let cpath = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid path to a new FIFO in the test's private directory.
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        assert!(open(&fifo, true).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
    }

    #[test]
    fn fallback_rename_never_replaces_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"original").unwrap();
        assert_eq!(
            rename_noreplace(&from, &to).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&to).unwrap(), b"original");
        fs::remove_file(&to).unwrap();
        rename_noreplace(&from, &to).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"new");
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn mutating_darwin_acl_grants_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow add_file,delete_child"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(parent(&dir.path().join("f")).is_err());
    }
}

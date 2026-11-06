//! Failure categories and their stable exit codes.

use std::fmt;

/// Stable failure categories. Each maps to a documented exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    /// Unexpected internal error.
    Internal,
    /// Invalid arguments, configuration, or input files.
    Usage,
    /// Network failure or exhausted retry budget.
    Network,
    /// The server or proxy refused the request (HTTP errors, authentication, TLS trust).
    Remote,
    /// Local storage failure (permissions, disk full, I/O errors).
    Storage,
    /// The downloaded data did not match the expected checksum.
    Integrity,
    /// Existing files or a changed source prevent safe continuation.
    Conflict,
    /// Stopped by a signal or a closed event stream; carries the exit code.
    Interrupted(u8),
}

impl Category {
    pub fn exit_code(self) -> u8 {
        match self {
            Category::Internal => 1,
            Category::Usage => 2,
            Category::Network => 3,
            Category::Remote => 4,
            Category::Storage => 5,
            Category::Integrity => 6,
            Category::Conflict => 7,
            Category::Interrupted(code) => code,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Category::Internal => "internal",
            Category::Usage => "usage",
            Category::Network => "network",
            Category::Remote => "remote",
            Category::Storage => "storage",
            Category::Integrity => "integrity",
            Category::Conflict => "conflict",
            Category::Interrupted(_) => "interrupted",
        }
    }
}

/// A user-facing failure: what went wrong and, when known, what to do next.
#[derive(Clone, Debug)]
pub struct Failure {
    pub category: Category,
    pub message: String,
    pub hint: Option<String>,
}

impl Failure {
    pub fn new(category: Category, message: impl Into<String>) -> Self {
        Failure {
            category,
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Failure::new(Category::Usage, message)
    }

    pub fn storage(message: impl Into<String>) -> Self {
        Failure::new(Category::Storage, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Failure::new(Category::Conflict, message)
    }

    pub fn remote(message: impl Into<String>) -> Self {
        Failure::new(Category::Remote, message)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

/// Describe an I/O error in a short phrase suitable for the terminal.
pub fn describe_io(err: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::PermissionDenied => "permission denied".into(),
        ErrorKind::NotFound => "no such file or directory".into(),
        ErrorKind::StorageFull => "no space left on device".into(),
        ErrorKind::ReadOnlyFilesystem => "read-only file system".into(),
        ErrorKind::QuotaExceeded => "disk quota exceeded".into(),
        _ => err.to_string(),
    }
}

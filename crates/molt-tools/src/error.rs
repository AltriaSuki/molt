//! Error replies. Their messages are read by the model, so they name the
//! path the request used and say what to do differently.

use std::io;

use molt_proto::{ErrorCode, RemoteError};

/// The request itself is wrong: bad input, or a path the caller may not use.
pub(crate) fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Invalid, message: message.into() }
}

/// The work failed.
pub(crate) fn failed(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Failed, message: message.into() }
}

/// An IO error on `shown` (the path as the request named it). A missing file
/// is the caller's mistake; anything else is a failure.
pub(crate) fn io(shown: &str, e: io::Error) -> RemoteError {
    match e.kind() {
        io::ErrorKind::NotFound => invalid(format!("{shown} does not exist")),
        io::ErrorKind::NotADirectory => invalid(format!("{shown}: a parent of it is not a directory")),
        _ => failed(format!("{shown}: {e}")),
    }
}

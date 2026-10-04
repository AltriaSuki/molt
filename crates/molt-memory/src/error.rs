use molt_proto::{ErrorCode, RemoteError};

pub(crate) fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Invalid, message: message.into() }
}

pub(crate) fn failed(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Failed, message: message.into() }
}

/// A database error as a `failed` reply.
pub(crate) fn db(e: rusqlite::Error) -> RemoteError {
    failed(format!("memory database: {e}"))
}

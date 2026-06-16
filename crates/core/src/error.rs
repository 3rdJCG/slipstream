use std::path::PathBuf;

/// Crate-wide result type.
pub type Result<T> = std::result::Result<T, Error>;

/// All errors crossing the core boundary. `serde::Serialize` so a future
/// `gui-tauri` can return these straight to the frontend as JSON.
#[derive(Debug, thiserror::Error, serde::Serialize)]
pub enum Error {
    #[error("unsupported log format for {0}")]
    UnsupportedFormat(PathBuf),

    #[error("failed to parse log: {0}")]
    Parse(String),

    #[error("unknown signal: {0}")]
    UnknownSignal(String),

    #[error("i/o error: {0}")]
    Io(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

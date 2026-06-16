//! Log ingestion: format detection + parsers producing [`FrameColumns`].
//!
//! Today both formats are read via the `blf_asc` crate (a single-threaded
//! message iterator). That is already far faster than the old Python tool; the
//! planned optimization is to parallelize BLF `LOG_CONTAINER` decompression
//! (see CLAUDE.md P0) once a real multi-GB sample is available to benchmark.

pub mod asc;
pub mod blf;

use std::path::Path;

use crate::model::{FrameColumns, LogFormat};
use crate::{Error, Result};

/// Sniff the format from the extension (later: magic bytes — BLF starts `LOGG`).
pub fn detect_format(path: &Path) -> Result<LogFormat> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("blf") => Ok(LogFormat::Blf),
        Some("asc") => Ok(LogFormat::Asc),
        _ => Err(Error::UnsupportedFormat(path.to_path_buf())),
    }
}

/// Parse any supported log into columnar frames (timestamps normalized to start
/// at 0.0 seconds — see [`normalize_timestamps`]).
pub fn parse(path: &Path) -> Result<FrameColumns> {
    let mut cols = match detect_format(path)? {
        LogFormat::Blf => blf::parse(path)?,
        LogFormat::Asc => asc::parse(path)?,
    };
    normalize_timestamps(&mut cols);
    Ok(cols)
}

/// Shift timestamps so the first frame is at t = 0. Log timestamps are
/// monotonically increasing, so subtracting the first is enough.
fn normalize_timestamps(cols: &mut FrameColumns) {
    if let Some(&t0) = cols.timestamp.first() {
        if t0 != 0.0 {
            for t in &mut cols.timestamp {
                *t -= t0;
            }
        }
    }
}

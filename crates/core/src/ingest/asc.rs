//! Vector ASC ingest — wraps `blf_asc::AscReader` into [`FrameColumns`].

use std::path::Path;

use blf_asc::AscReader;

use crate::model::FrameColumns;
use crate::{Error, Result};

pub fn parse(path: &Path) -> Result<FrameColumns> {
    let mut reader = AscReader::open(path).map_err(|e| Error::Parse(e.to_string()))?;
    let mut cols = FrameColumns::default();
    for msg in reader.by_ref() {
        cols.push(
            msg.timestamp,
            msg.channel as u8,
            msg.arbitration_id.0,
            msg.is_fd,
            msg.data.as_slice(),
        );
    }
    if let Some(e) = reader.take_error() {
        return Err(Error::Parse(e.to_string()));
    }
    Ok(cols)
}

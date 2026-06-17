//! Columnar store over [`FrameColumns`] + (planned) Parquet cache.
//!
//! On first ingest the real impl writes a Parquet sidecar so re-opening a
//! multi-GB log is instant (no re-parse). The store also serves *windows* of
//! rows to the table view — only the visible slice is ever materialized.

use std::path::Path;

use crate::model::{FrameColumns, FrameRow};
use crate::Result;

pub struct FrameStore {
    frames: FrameColumns,
}

impl FrameStore {
    pub fn new(frames: FrameColumns) -> Self {
        Self { frames }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn columns(&self) -> &FrameColumns {
        &self.frames
    }

    /// Materialize one row (hex-formats the payload).
    pub fn row(&self, i: usize) -> FrameRow {
        let dlc = self.frames.dlc[i];
        let data = self.frames.data[i][..dlc as usize]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();
        FrameRow {
            index: i as u64,
            timestamp: self.frames.timestamp[i],
            channel: self.frames.channel[i],
            can_id: self.frames.can_id[i],
            is_extended: self.frames.is_extended[i],
            is_fd: self.frames.is_fd[i],
            dlc,
            data,
        }
    }

    /// A contiguous window of rows for the (virtualized) table view.
    pub fn window(&self, start: usize, count: usize) -> Vec<FrameRow> {
        let end = (start + count).min(self.len());
        (start..end).map(|i| self.row(i)).collect()
    }

    /// Materialize a window of rows from an explicit list of row indices (e.g. a
    /// filtered index list). The returned `index` field keeps each row's
    /// position in the *full* store, so the table can still address frames.
    pub fn window_of(&self, indices: &[usize]) -> Vec<FrameRow> {
        indices.iter().map(|&i| self.row(i)).collect()
    }

    // TODO(P0): Parquet cache sidecar.
    pub fn save_cache(&self, _path: &Path) -> Result<()> {
        Ok(())
    }
}

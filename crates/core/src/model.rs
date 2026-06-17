//! Neutral domain types. Serde-friendly, no GUI deps.

use serde::{Deserialize, Serialize};

/// Source log formats we ingest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogFormat {
    /// Vector binary log (zlib `LOG_CONTAINER`s).
    Blf,
    /// Vector ASCII log (text).
    Asc,
}

/// Columnar raw-frame storage — **struct-of-arrays**. Each `Vec` is one column;
/// row `i` is `(timestamp[i], channel[i], can_id[i], ...)`.
///
/// SoA (not `Vec<Frame>`) is what makes vectorized signal decoding and Polars
/// hand-off cheap. For the real ingest, `data` should become a flat `Vec<u8>`
/// plus per-row offsets to avoid the fixed 64-byte stride; kept simple here.
#[derive(Debug, Default, Clone)]
pub struct FrameColumns {
    /// Seconds since log start.
    pub timestamp: Vec<f64>,
    pub channel: Vec<u8>,
    pub can_id: Vec<u32>,
    /// `true` for 29-bit extended ids, `false` for 11-bit standard ids.
    pub is_extended: Vec<bool>,
    pub is_fd: Vec<bool>,
    /// Payload length in bytes (0..=64).
    pub dlc: Vec<u8>,
    /// Payload, zero-padded to 64 bytes (CAN-FD max).
    pub data: Vec<[u8; 64]>,
}

impl FrameColumns {
    pub fn len(&self) -> usize {
        self.timestamp.len()
    }

    pub fn is_empty(&self) -> bool {
        self.timestamp.is_empty()
    }

    pub fn push(
        &mut self,
        ts: f64,
        channel: u8,
        can_id: u32,
        is_extended: bool,
        is_fd: bool,
        payload: &[u8],
    ) {
        let mut data = [0u8; 64];
        let n = payload.len().min(64);
        data[..n].copy_from_slice(&payload[..n]);
        self.timestamp.push(ts);
        self.channel.push(channel);
        self.can_id.push(can_id);
        self.is_extended.push(is_extended);
        self.is_fd.push(is_fd);
        self.dlc.push(n as u8);
        self.data.push(data);
    }

    /// Append all rows of `other` (used to combine multiple loaded logs).
    pub fn append(&mut self, other: &FrameColumns) {
        self.timestamp.extend_from_slice(&other.timestamp);
        self.channel.extend_from_slice(&other.channel);
        self.can_id.extend_from_slice(&other.can_id);
        self.is_extended.extend_from_slice(&other.is_extended);
        self.is_fd.extend_from_slice(&other.is_fd);
        self.dlc.extend_from_slice(&other.dlc);
        self.data.extend_from_slice(&other.data);
    }

    /// Stable-sort all columns by timestamp (used after merging logs whose time
    /// bases interleave). Single-log ingest is already time-ordered.
    pub fn sort_by_timestamp(&mut self) {
        let mut idx: Vec<usize> = (0..self.len()).collect();
        idx.sort_by(|&a, &b| {
            self.timestamp[a]
                .partial_cmp(&self.timestamp[b])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.timestamp = idx.iter().map(|&i| self.timestamp[i]).collect();
        self.channel = idx.iter().map(|&i| self.channel[i]).collect();
        self.can_id = idx.iter().map(|&i| self.can_id[i]).collect();
        self.is_extended = idx.iter().map(|&i| self.is_extended[i]).collect();
        self.is_fd = idx.iter().map(|&i| self.is_fd[i]).collect();
        self.dlc = idx.iter().map(|&i| self.dlc[i]).collect();
        self.data = idx.iter().map(|&i| self.data[i]).collect();
    }
}

/// One raw frame, materialized for a table row window (serde wire type).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameRow {
    pub index: u64,
    pub timestamp: f64,
    pub channel: u8,
    pub can_id: u32,
    /// `true` for 29-bit extended ids, `false` for 11-bit standard ids.
    pub is_extended: bool,
    pub is_fd: bool,
    pub dlc: u8,
    /// Hex bytes, e.g. `["1A", "FF", ...]`.
    pub data: Vec<String>,
}

/// Bit layout of a DBC signal (byte order matters for decode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ByteOrder {
    /// Little-endian (Intel).
    Intel,
    /// Big-endian (Motorola).
    Motorola,
}

/// Metadata for one decodable signal, surfaced to the signal tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalMeta {
    pub name: String,
    /// Owning message frame id.
    pub can_id: u32,
    pub message: String,
    pub unit: String,
    /// Channel the owning DBC is scoped to (`None` = all channels).
    pub channel: Option<u8>,
}

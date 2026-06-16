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

    pub fn push(&mut self, ts: f64, channel: u8, can_id: u32, is_fd: bool, payload: &[u8]) {
        let mut data = [0u8; 64];
        let n = payload.len().min(64);
        data[..n].copy_from_slice(&payload[..n]);
        self.timestamp.push(ts);
        self.channel.push(channel);
        self.can_id.push(can_id);
        self.is_fd.push(is_fd);
        self.dlc.push(n as u8);
        self.data.push(data);
    }
}

/// One raw frame, materialized for a table row window (serde wire type).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameRow {
    pub index: u64,
    pub timestamp: f64,
    pub channel: u8,
    pub can_id: u32,
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
}

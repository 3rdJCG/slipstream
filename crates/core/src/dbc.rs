//! DBC database loading (`can-dbc`) + signal bit-decode.
//!
//! Decode is **column-at-a-time** at the call site (see `query::signal_series`):
//! gather all frames for a `can_id`, then run [`decode_signal`] over each. The
//! bit-extraction here handles Intel/Motorola layout, signed-ness, and the
//! linear `raw * scale + offset` conversion.
//!
//! TODO(P1): multiplexed signals are decoded unconditionally for now; honor the
//! multiplexor selector so multiplexed signals are only decoded on matching
//! frames.

use std::path::Path;

use crate::model::{ByteOrder, SignalMeta};
use crate::{Error, Result};

/// One signal's bit layout + linear conversion.
#[derive(Debug, Clone)]
pub struct SignalDef {
    pub name: String,
    pub start_bit: u16,
    pub bit_len: u16,
    pub byte_order: ByteOrder,
    pub signed: bool,
    pub scale: f64,
    pub offset: f64,
    pub unit: String,
}

/// Parsed DBC: messages keyed by frame id.
#[derive(Debug, Default, Clone)]
pub struct DbcDatabase {
    pub messages: Vec<DbcMessage>,
}

#[derive(Debug, Clone)]
pub struct DbcMessage {
    pub can_id: u32,
    pub name: String,
    pub signals: Vec<SignalDef>,
}

impl DbcDatabase {
    /// Load and parse a `.dbc` file (UTF-8).
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    /// Parse DBC text.
    pub fn parse(text: &str) -> Result<Self> {
        let dbc = can_dbc::Dbc::try_from(text)
            .map_err(|e| Error::Parse(format!("DBC parse failed: {e:?}")))?;
        let messages = dbc
            .messages
            .iter()
            .map(|m| DbcMessage {
                // Strip the extended-id flag; frame ids from ingest are 29-bit.
                can_id: m.id.raw() & 0x1FFF_FFFF,
                name: m.name.clone(),
                signals: m.signals.iter().map(map_signal).collect(),
            })
            .collect();
        Ok(Self { messages })
    }

    /// Flatten to signal metadata for the UI signal tree.
    pub fn signal_metas(&self) -> Vec<SignalMeta> {
        self.messages
            .iter()
            .flat_map(|m| {
                m.signals.iter().map(move |s| SignalMeta {
                    name: s.name.clone(),
                    can_id: m.can_id,
                    message: m.name.clone(),
                    unit: s.unit.clone(),
                })
            })
            .collect()
    }

    /// Find a signal by name, returning it with its owning message id.
    pub fn find_signal(&self, name: &str) -> Option<(u32, &SignalDef)> {
        self.messages.iter().find_map(|m| {
            m.signals
                .iter()
                .find(|s| s.name == name)
                .map(|s| (m.can_id, s))
        })
    }
}

fn map_signal(s: &can_dbc::Signal) -> SignalDef {
    SignalDef {
        name: s.name.clone(),
        start_bit: s.start_bit as u16,
        bit_len: s.size as u16,
        byte_order: match s.byte_order {
            can_dbc::ByteOrder::LittleEndian => ByteOrder::Intel,
            can_dbc::ByteOrder::BigEndian => ByteOrder::Motorola,
        },
        signed: matches!(s.value_type, can_dbc::ValueType::Signed),
        scale: s.factor,
        offset: s.offset,
        unit: s.unit.clone(),
    }
}

/// Decode one signal's physical value from a frame payload, or `None` if the
/// signal's bits fall outside `data`.
pub fn decode_signal(sig: &SignalDef, data: &[u8]) -> Option<f64> {
    let raw = extract_bits(
        data,
        sig.start_bit as usize,
        sig.bit_len as usize,
        sig.byte_order,
    )?;
    let value = if sig.signed {
        sign_extend(raw, sig.bit_len as usize) as f64
    } else {
        raw as f64
    };
    Some(value * sig.scale + sig.offset)
}

/// Extract `len` bits as an unsigned integer.
///
/// Intel (little-endian): `start` is the LSB; bits ascend.
/// Motorola (big-endian): `start` is the MSB; bits descend, wrapping to bit 7
/// of the next byte when a byte boundary is crossed (DBC "sawtooth" numbering).
fn extract_bits(data: &[u8], start: usize, len: usize, order: ByteOrder) -> Option<u64> {
    if len == 0 || len > 64 {
        return None;
    }
    let mut result: u64 = 0;
    match order {
        ByteOrder::Intel => {
            for i in 0..len {
                let bit_pos = start + i;
                let byte = *data.get(bit_pos / 8)?;
                let bit = (byte >> (bit_pos % 8)) & 1;
                result |= (bit as u64) << i;
            }
        }
        ByteOrder::Motorola => {
            let mut bit_pos = start;
            for _ in 0..len {
                let byte = *data.get(bit_pos / 8)?;
                let bit = (byte >> (bit_pos % 8)) & 1;
                result = (result << 1) | bit as u64;
                if bit_pos % 8 == 0 {
                    // Cross to bit 7 of the next byte.
                    bit_pos += 15;
                } else {
                    bit_pos -= 1;
                }
            }
        }
    }
    Some(result)
}

/// Interpret the low `len` bits of `raw` as a two's-complement signed integer.
fn sign_extend(raw: u64, len: usize) -> i64 {
    if len >= 64 {
        return raw as i64;
    }
    let sign_bit = 1u64 << (len - 1);
    if raw & sign_bit != 0 {
        (raw | !((1u64 << len) - 1)) as i64
    } else {
        raw as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(start: u16, len: u16, order: ByteOrder, signed: bool, scale: f64, offset: f64) -> SignalDef {
        SignalDef {
            name: "S".into(),
            start_bit: start,
            bit_len: len,
            byte_order: order,
            signed,
            scale,
            offset,
            unit: "".into(),
        }
    }

    #[test]
    fn intel_unsigned_16() {
        let s = sig(0, 16, ByteOrder::Intel, false, 1.0, 0.0);
        // little-endian: 0x1234 stored as [0x34, 0x12]
        assert_eq!(decode_signal(&s, &[0x34, 0x12]), Some(4660.0));
    }

    #[test]
    fn intel_scale_offset() {
        let s = sig(0, 16, ByteOrder::Intel, false, 0.1, -5.0);
        assert_eq!(decode_signal(&s, &[0x34, 0x12]), Some(4660.0 * 0.1 - 5.0));
    }

    #[test]
    fn motorola_unsigned_16() {
        let s = sig(7, 16, ByteOrder::Motorola, false, 1.0, 0.0);
        // big-endian: 0x1234 stored as [0x12, 0x34]
        assert_eq!(decode_signal(&s, &[0x12, 0x34]), Some(4660.0));
    }

    #[test]
    fn intel_signed_8() {
        let s = sig(0, 8, ByteOrder::Intel, true, 1.0, 0.0);
        assert_eq!(decode_signal(&s, &[0xFF]), Some(-1.0));
        assert_eq!(decode_signal(&s, &[0x80]), Some(-128.0));
        assert_eq!(decode_signal(&s, &[0x7F]), Some(127.0));
    }

    #[test]
    fn out_of_bounds_returns_none() {
        let s = sig(0, 16, ByteOrder::Intel, false, 1.0, 0.0);
        assert_eq!(decode_signal(&s, &[0x00]), None); // needs 2 bytes
    }

    #[test]
    fn parse_minimal_dbc() {
        let dbc = "VERSION \"\"\n\nBO_ 256 EngineData: 8 ECU\n SG_ Rpm : 0|16@1+ (0.25,0) [0|16383] \"rpm\" Vector__XXX\n";
        let db = DbcDatabase::parse(dbc).expect("parse");
        assert_eq!(db.messages.len(), 1);
        assert_eq!(db.messages[0].can_id, 256);
        let (id, s) = db.find_signal("Rpm").expect("signal");
        assert_eq!(id, 256);
        assert_eq!(s.bit_len, 16);
        assert_eq!(s.scale, 0.25);
    }
}

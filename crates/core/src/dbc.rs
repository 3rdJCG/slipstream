//! DBC database loading (`can-dbc`) + signal bit-decode.
//!
//! Decode is **column-at-a-time** at the call site (see `query::signal_series`):
//! gather all frames for a `can_id`, then run [`decode_signal`] over each. The
//! bit-extraction here handles Intel/Motorola layout, signed-ness, and the
//! linear `raw * scale + offset` conversion.
//!
//! Multiplexed signals honor the multiplexor selector: a signal tagged
//! [`MuxRole::Multiplexed(sel)`] is only decoded on frames where its message's
//! multiplexor signal reads `sel` (see `query::signal_series`).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{ByteOrder, SignalMeta};
use crate::{Error, Result};

/// A signal's role in a message's multiplexing scheme.
///
/// DBC's `MultiplexorAndMultiplexedSignal` (a signal that is *both* multiplexed
/// and itself a sub-multiplexor) is simplified to [`MuxRole::Multiplexor`]: we
/// treat it as the selector and do not model nested multiplexing. Plain
/// messages have no multiplexor and all their signals are [`MuxRole::Plain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MuxRole {
    /// An ordinary, always-present signal.
    Plain,
    /// The multiplexor switch — its value selects which multiplexed signals are
    /// present in a given frame.
    Multiplexor,
    /// Present only when the message's multiplexor reads this selector value.
    Multiplexed(u64),
}

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
    /// Role in the owning message's multiplexing scheme.
    pub mux: MuxRole,
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
    /// Expected cycle time in milliseconds, from the `GenMsgCycleTime`
    /// attribute, if the DBC declares one.
    pub expected_cycle_ms: Option<f64>,
}

impl DbcDatabase {
    /// Load and parse a `.dbc` file.
    ///
    /// Reads raw bytes and decodes tolerantly: UTF-8 first, falling back to
    /// Windows-1252 (CP1252) when the bytes are not valid UTF-8. Real-world DBC
    /// files are frequently authored in CP1252 (e.g. `°`, `é` in signal units).
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let text: std::borrow::Cow<str> = match std::str::from_utf8(&bytes) {
            Ok(s) => std::borrow::Cow::Borrowed(s),
            Err(_) => can_dbc::decode_cp1252(&bytes).ok_or_else(|| {
                Error::Parse("DBC file is neither valid UTF-8 nor CP1252".into())
            })?,
        };
        Self::parse(&text)
    }

    /// Parse DBC text.
    pub fn parse(text: &str) -> Result<Self> {
        let dbc = can_dbc::Dbc::try_from(text)
            .map_err(|e| Error::Parse(format!("DBC parse failed: {e:?}")))?;

        // Collect GenMsgCycleTime (ms) per message id, if present.
        let mut cycle_ms: std::collections::HashMap<u32, f64> = std::collections::HashMap::new();
        for av in &dbc.attribute_values_message {
            if av.name != "GenMsgCycleTime" {
                continue;
            }
            let ms = match av.value {
                can_dbc::AttributeValue::Uint(u) => u as f64,
                can_dbc::AttributeValue::Int(i) => i as f64,
                can_dbc::AttributeValue::Double(d) => d,
                can_dbc::AttributeValue::String(_) => continue,
            };
            if ms > 0.0 {
                cycle_ms.insert(av.message_id.raw() & 0x1FFF_FFFF, ms);
            }
        }

        let messages = dbc
            .messages
            .iter()
            .map(|m| {
                // Strip the extended-id flag; frame ids from ingest are 29-bit.
                let can_id = m.id.raw() & 0x1FFF_FFFF;
                DbcMessage {
                    can_id,
                    name: m.name.clone(),
                    signals: m.signals.iter().map(map_signal).collect(),
                    expected_cycle_ms: cycle_ms.get(&can_id).copied(),
                }
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
                    channel: None,
                })
            })
            .collect()
    }

    /// Find a signal by name, returning it together with its owning message.
    /// The message is needed so the decode path can locate the message's
    /// multiplexor signal (see [`DbcMessage::multiplexor`]).
    pub fn find_signal(&self, name: &str) -> Option<(&DbcMessage, &SignalDef)> {
        self.messages.iter().find_map(|m| {
            m.signals
                .iter()
                .find(|s| s.name == name)
                .map(|s| (m, s))
        })
    }
}

impl DbcMessage {
    /// The message's multiplexor (selector) signal, if it has one.
    pub fn multiplexor(&self) -> Option<&SignalDef> {
        self.signals.iter().find(|s| s.mux == MuxRole::Multiplexor)
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
        mux: match s.multiplexer_indicator {
            can_dbc::MultiplexIndicator::Plain => MuxRole::Plain,
            can_dbc::MultiplexIndicator::Multiplexor => MuxRole::Multiplexor,
            can_dbc::MultiplexIndicator::MultiplexedSignal(sel) => MuxRole::Multiplexed(sel),
            // Simplify a combined multiplexor+multiplexed signal to the
            // multiplexor role (we do not model nested multiplexing).
            can_dbc::MultiplexIndicator::MultiplexorAndMultiplexedSignal(_) => MuxRole::Multiplexor,
        },
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
            mux: MuxRole::Plain,
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
        let (m, s) = db.find_signal("Rpm").expect("signal");
        assert_eq!(m.can_id, 256);
        assert_eq!(s.bit_len, 16);
        assert_eq!(s.scale, 0.25);
        assert_eq!(s.mux, MuxRole::Plain);
    }

    #[test]
    fn load_cp1252_dbc_falls_back() {
        // Build a valid DBC whose signal unit contains a CP1252-only byte
        // (0xB0 = '°' in CP1252, which is *not* valid UTF-8 on its own).
        let mut bytes: Vec<u8> =
            b"VERSION \"\"\n\nBO_ 256 EngineData: 8 ECU\n SG_ Temp : 0|16@1+ (0.1,0) [0|6553] \""
                .to_vec();
        bytes.push(0xB0); // '°' in CP1252
        bytes.push(b'C');
        bytes.extend_from_slice(b"\" Vector__XXX\n");

        // Sanity: these bytes are not valid UTF-8, so load() must fall back.
        assert!(std::str::from_utf8(&bytes).is_err());

        let mut path = std::env::temp_dir();
        path.push(format!("slipstream_cp1252_test_{}.dbc", std::process::id()));
        std::fs::write(&path, &bytes).expect("write temp dbc");

        let result = DbcDatabase::load(&path);
        let _ = std::fs::remove_file(&path); // clean up regardless of outcome

        let db = result.expect("load should succeed via CP1252 fallback");
        let (_, s) = db.find_signal("Temp").expect("signal Temp");
        // The CP1252 byte 0xB0 decodes to '°', so the unit is "°C".
        assert_eq!(s.unit, "\u{00B0}C");
    }

    #[test]
    fn parse_multiplexed_dbc_assigns_roles() {
        // A message with a multiplexor `Mux` (M) and two multiplexed signals
        // `A` (m0) and `B` (m1).
        let dbc = "VERSION \"\"\n\nBO_ 512 Muxed: 8 ECU\n SG_ Mux M : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n SG_ A m0 : 8|8@1+ (1,0) [0|255] \"\" Vector__XXX\n SG_ B m1 : 8|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";
        let db = DbcDatabase::parse(dbc).expect("parse");
        let (m, mux) = db.find_signal("Mux").expect("Mux");
        assert_eq!(mux.mux, MuxRole::Multiplexor);
        assert_eq!(m.multiplexor().map(|s| s.name.as_str()), Some("Mux"));
        let (_, a) = db.find_signal("A").expect("A");
        assert_eq!(a.mux, MuxRole::Multiplexed(0));
        let (_, b) = db.find_signal("B").expect("B");
        assert_eq!(b.mux, MuxRole::Multiplexed(1));
    }
}

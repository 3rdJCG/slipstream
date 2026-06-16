//! Vector ASC ingest — a hand-written, *tolerant* line parser.
//!
//! Unlike `blf_asc::AscReader` (which hard-errors on the first line it does not
//! understand and then stops), this parser **never** fails on a malformed or
//! unrecognized line: it skips it and keeps reading. Only unreadable file I/O
//! is reported as an error. This lets real-world logs — peppered with
//! `Statistic:` / `Status:` / `J1939TP` / `ErrorFrame` / named CAN-FD lines —
//! ingest cleanly instead of failing on the first oddity.

use std::path::Path;

use crate::model::FrameColumns;
use crate::{Error, Result};

/// Numeric base for ids / dlc / data bytes. Defaults to hex; a `base dec`
/// header line flips it to decimal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Base {
    Hex,
    Dec,
}

impl Base {
    fn radix(self) -> u32 {
        match self {
            Base::Hex => 16,
            Base::Dec => 10,
        }
    }

    fn parse(self, tok: &str) -> Option<u64> {
        u64::from_str_radix(tok, self.radix()).ok()
    }
}

/// True for meta/noise lines that carry no frame and must be skipped.
fn is_noise(line: &str) -> bool {
    let t = line.trim_start();
    if t.is_empty() {
        return true;
    }
    // Prefix-based meta lines.
    const PREFIXES: &[&str] = &[
        "//",
        "date ",
        "base ",
        "internal events logged",
        "Begin Triggerblock",
        "End TriggerBlock",
        "Start of measurement",
        "previous log file",
    ];
    if PREFIXES.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    // Substring markers that can appear after the timestamp/channel.
    t.contains("Status:") || t.contains("Statistic:") || t.contains("J1939TP")
}

pub fn parse(path: &Path) -> Result<FrameColumns> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io(e.to_string()))?;
    let mut cols = FrameColumns::default();
    let mut base = Base::Hex;

    for raw in text.lines() {
        // A `base dec` / `base hex` header flips the numeric base for following
        // lines (default hex). Detect before treating it as noise.
        let trimmed = raw.trim_start();
        if let Some(rest) = trimmed.strip_prefix("base ") {
            if rest.trim_start().to_ascii_lowercase().starts_with("dec") {
                base = Base::Dec;
            } else {
                base = Base::Hex;
            }
            continue;
        }
        if is_noise(raw) {
            continue;
        }
        // A frame line, if recognized, is pushed; anything unparseable is skipped.
        parse_frame_line(raw, base, &mut cols);
    }

    Ok(cols)
}

/// Try to parse one line as a frame and push it. Never panics; on any missing
/// or malformed field it simply returns without pushing.
fn parse_frame_line(line: &str, base: Base, cols: &mut FrameColumns) {
    let toks: Vec<&str> = line.split_whitespace().collect();
    if toks.len() < 2 {
        return;
    }

    // Token 0 is always the timestamp (decimal seconds, regardless of base).
    let ts: f64 = match toks[0].parse() {
        Ok(v) => v,
        Err(_) => return,
    };

    if toks[1].eq_ignore_ascii_case("CANFD") {
        parse_canfd(&toks, ts, base, cols);
    } else {
        parse_classic(&toks, ts, base, cols);
    }
}

/// Classic CAN data/remote line:
///   <ts> <channel> <id>[x] <Rx|Tx> <d|r> <dlc> <data...> [trailing fields]
fn parse_classic(toks: &[&str], ts: f64, base: Base, cols: &mut FrameColumns) {
    // toks[1] = channel (always decimal).
    let channel: u32 = match toks[1].parse() {
        Ok(v) => v,
        Err(_) => return,
    };
    // toks[2] = arbitration id with optional trailing 'x' (extended).
    let id_tok = match toks.get(2) {
        Some(t) => *t,
        None => return,
    };
    // "ErrorFrame" and other non-id markers are not frames we keep.
    if id_tok.eq_ignore_ascii_case("ErrorFrame") {
        return;
    }
    let (can_id, _extended) = match parse_id(id_tok, base) {
        Some(v) => v,
        None => return,
    };

    // toks[3] = direction (Rx/Tx). Required to anchor the format.
    match toks.get(3) {
        Some(d) if d.eq_ignore_ascii_case("Rx") || d.eq_ignore_ascii_case("Tx") => {}
        _ => return,
    }

    // toks[4] = type: 'd' (data) or 'r' (remote).
    let kind = match toks.get(4) {
        Some(k) => *k,
        None => return,
    };

    if kind.eq_ignore_ascii_case("r") {
        // Remote frame: no payload (a dlc may follow but we ignore the data).
        cols.push(ts, channel as u8, can_id, false, &[]);
        return;
    }
    if !kind.eq_ignore_ascii_case("d") {
        return; // unknown frame type → skip.
    }

    // toks[5] = dlc (in `base`); data bytes follow.
    let dlc = match toks.get(5).and_then(|t| base.parse(t)) {
        Some(v) => v as usize,
        None => return,
    };
    let want = dlc.min(8);
    let data = collect_bytes(&toks[6..], want, base);
    cols.push(ts, channel as u8, can_id, false, &data);
}

/// CAN-FD line:
///   <ts> CANFD <channel> <Rx|Tx> <id>[x] [<symbolic_name>] <brs> <esi> <dlc>
///   <datalen> <data... (datalen)> <trailing fields>
fn parse_canfd(toks: &[&str], ts: f64, base: Base, cols: &mut FrameColumns) {
    // toks[2] = channel (decimal).
    let channel: u32 = match toks.get(2).and_then(|t| t.parse().ok()) {
        Some(v) => v,
        None => return,
    };
    // toks[3] = direction.
    match toks.get(3) {
        Some(d) if d.eq_ignore_ascii_case("Rx") || d.eq_ignore_ascii_case("Tx") => {}
        _ => return,
    }
    // toks[4] = id (or "ErrorFrame" marker → skip the whole line).
    let id_tok = match toks.get(4) {
        Some(t) => *t,
        None => return,
    };
    if id_tok.eq_ignore_ascii_case("ErrorFrame") {
        return;
    }
    let (can_id, _extended) = match parse_id(id_tok, base) {
        Some(v) => v,
        None => return,
    };

    // After the id comes an optional symbolic name, then brs/esi (lone 0/1
    // digits). Skip a name token if present: it is a token that is NOT a lone
    // 0/1 digit.
    let mut i = 5;
    if let Some(t) = toks.get(i) {
        if !is_bit_flag(t) {
            i += 1; // consume symbolic name.
        }
    }

    // brs, esi: two flag tokens.
    let _brs = toks.get(i);
    let _esi = toks.get(i + 1);
    i += 2;

    // dlc (in base), datalen (decimal byte count).
    let _dlc = match toks.get(i).and_then(|t| base.parse(t)) {
        Some(v) => v,
        None => return,
    };
    i += 1;
    let datalen = match toks.get(i).and_then(|t| t.parse::<usize>().ok()) {
        Some(v) => v,
        None => return,
    };
    i += 1;

    let want = datalen.min(64);
    let data = collect_bytes(&toks[i.min(toks.len())..], want, base);
    cols.push(ts, channel as u8, can_id, true, &data);
}

/// Parse an arbitration id token, honoring a trailing `x` (extended → masked to
/// 29 bits). Returns `(can_id, is_extended)`.
fn parse_id(tok: &str, base: Base) -> Option<(u32, bool)> {
    let (digits, extended) = match tok.strip_suffix(['x', 'X']) {
        Some(d) => (d, true),
        None => (tok, false),
    };
    let raw = base.parse(digits)? as u32;
    let id = if extended { raw & 0x1FFF_FFFF } else { raw };
    Some((id, extended))
}

/// A lone `0` or `1` token (CAN-FD brs/esi flag).
fn is_bit_flag(tok: &str) -> bool {
    tok == "0" || tok == "1"
}

/// Read up to `want` data bytes from the leading tokens, stopping at the first
/// token that is not a valid byte in `base` (e.g. a trailing `Length=` field).
fn collect_bytes(toks: &[&str], want: usize, base: Base) -> Vec<u8> {
    let mut out = Vec::with_capacity(want);
    for &t in toks {
        if out.len() == want {
            break;
        }
        match base.parse(t) {
            Some(v) if v <= 0xFF => out.push(v as u8),
            _ => break, // hit a trailing non-data field.
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative log: header lines, Status/Statistic/J1939TP/ErrorFrame
    /// noise, classic standard + extended data lines (with trailing fields), a
    /// remote line, and CAN-FD lines with and without a symbolic name.
    const SAMPLE: &str = "\
date Sam Sep 30 15:06:13.191 2017
base hex  timestamps absolute
internal events logged
// version 9.0.0
Begin Triggerblock Sam Sep 30 15:06:13.191 2017
   0.000000 Start of measurement
   0.015991 CAN 1 Status:chip status error passive - TxErr: 132 RxErr: 0
   1.015991 1  Statistic: D 0 R 0 XD 0 XR 0 E 0 O 0 B 0.00%
   2.501000 1 ErrorFrame
   2.501010 1 ErrorFrame ECC: 10100010
   2.510001 2 100 Tx r
   3.098426 1  18EBFF00x       Rx d 8 01 A0 0F A6 60 3B D1 40    Length = 273910 BitCount = 141 ID = 418119424x
   3.297693 1  J1939TP FEE3p        6    0  0   -   Rx   d 23 A0 0F A6 60 3B D1 40
  17.876708 1  6F9             Rx   d 8 05 0C 00 00 00 00 00 00  Length = 240015 BitCount = 124 ID = 1785
  30.005071 CANFD   2 Rx        300  Generic_Name_12                  1 0 8  8 01 02 03 04 05 06 07 08   102203  133   303000 e0006659
  30.300981 CANFD   3 Tx     50005x                                0 0 5 0 140000 73 200050 7a60
  30.806898 CANFD   5 Tx ErrorFrame Not Acknowledge error 44 0 0 f 64 00 00
End TriggerBlock
";

    fn write_tmp(name: &str, body: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("slipstream_asc_test_{name}.asc"));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn tolerant_parse_skips_noise_keeps_frames() {
        let path = write_tmp("tolerant", SAMPLE);
        let cols = parse(&path).expect("parse must succeed despite noise");

        // Real data/remote frames only:
        //   classic remote (100), classic data (18EBFF00x), classic data (6F9),
        //   CANFD (300 w/ name), CANFD (50005x w/o name).
        // J1939TP, Status, Statistic, ErrorFrame (classic + CANFD) are skipped.
        assert_eq!(cols.len(), 5, "frame count (noise skipped)");

        // Frame 0: remote 0x100 on channel 2, no payload.
        assert_eq!(cols.can_id[0], 0x100);
        assert!(!cols.is_fd[0]);
        assert_eq!(cols.dlc[0], 0);

        // Frame 1: extended 0x18EBFF00 (29-bit), first byte 0x01, 8 bytes.
        assert_eq!(cols.can_id[1], 0x18EBFF00);
        assert!(!cols.is_fd[1]);
        assert_eq!(cols.dlc[1], 8);
        assert_eq!(cols.data[1][0], 0x01);

        // Frame 2: standard 0x6F9, first byte 0x05.
        assert_eq!(cols.can_id[2], 0x6F9);
        assert_eq!(cols.data[2][0], 0x05);

        // Frame 3: CAN-FD 0x300 with symbolic name, brs=1, dlc=8, first byte 0x01.
        assert_eq!(cols.can_id[3], 0x300);
        assert!(cols.is_fd[3]);
        assert_eq!(cols.dlc[3], 8);
        assert_eq!(cols.data[3][0], 0x01);

        // Frame 4: CAN-FD extended 0x50005 (no name), datalen 0 → no payload.
        assert_eq!(cols.can_id[4], 0x50005);
        assert!(cols.is_fd[4]);
        assert_eq!(cols.dlc[4], 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn base_dec_header_switches_radix() {
        // With "base dec", ids and bytes are decimal.
        let body = "\
date x
base dec  timestamps absolute
   0.000000 Start of measurement
   1.000000 1  100             Rx   d 2 10 20
";
        let path = write_tmp("basedec", body);
        let cols = parse(&path).expect("parse");
        assert_eq!(cols.len(), 1);
        assert_eq!(cols.can_id[0], 100); // decimal, not 0x100
        assert_eq!(cols.dlc[0], 2);
        assert_eq!(cols.data[0][0], 10);
        assert_eq!(cols.data[0][1], 20);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn writer_format_classic_line() {
        // Mirrors blf_asc::AscWriter output: "  0.000000 1  123    Rx   d 8 .."
        let body = "\
date x
base hex  timestamps absolute
   0.000000 1  123    Rx   d 4 DE AD BE EF
   0.005000 1  7FF    Rx   d 1 11
";
        let path = write_tmp("writerfmt", body);
        let cols = parse(&path).expect("parse");
        assert_eq!(cols.len(), 2);
        assert_eq!(cols.can_id, vec![0x123, 0x7FF]);
        assert_eq!(&cols.data[0][..4], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(cols.data[1][0], 0x11);
        let _ = std::fs::remove_file(&path);
    }
}

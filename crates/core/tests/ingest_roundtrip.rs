//! Round-trip ingest tests: synthesize a log with `blf_asc`'s writers, then read
//! it back through `slipstream_core::ingest` and assert the columns match.
//!
//! NOTE: this validates the ingest *machinery* (container inflate, object
//! iteration, field mapping) against the same library's writer. Validation
//! against real Vector-tool `.blf`/`.asc` files is still pending a sample
//! (tracked in CLAUDE.md).

use std::path::PathBuf;

use blf_asc::{AscWriter, BlfWriter, Message};
use slipstream_core::ingest;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("slipstream_test_{name}"))
}

fn msg(ts: f64, id: u32, data: &[u8]) -> Message {
    Message {
        timestamp: ts,
        arbitration_id: id.into(),
        dlc: data.len() as u8,
        data: data.to_vec().into(),
        channel: 0,
        ..Default::default()
    }
}

#[test]
fn blf_round_trip() {
    let path = tmp("rt.blf");
    {
        let mut w = BlfWriter::create(&path).expect("create blf");
        w.on_message_received(&msg(0.0, 0x100, &[1, 2, 3, 4, 5, 6, 7, 8]))
            .unwrap();
        w.on_message_received(&msg(0.01, 0x200, &[0xAA, 0xBB]))
            .unwrap();
        w.on_message_received(&msg(0.02, 0x100, &[9])).unwrap();
        w.finish().unwrap();
    }

    let cols = ingest::parse(&path).expect("ingest blf");
    assert_eq!(cols.len(), 3, "frame count");
    // Timestamps are normalized to start at 0.
    assert_eq!(cols.timestamp[0], 0.0);
    assert!(cols.timestamp[2] > cols.timestamp[0]);
    assert_eq!(cols.can_id, vec![0x100, 0x200, 0x100]);
    assert_eq!(cols.dlc, vec![8, 2, 1]);
    assert_eq!(&cols.data[0][..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(&cols.data[1][..2], &[0xAA, 0xBB]);
    assert_eq!(cols.data[2][0], 9);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn asc_round_trip() {
    let path = tmp("rt.asc");
    {
        let mut w = AscWriter::create(&path).expect("create asc");
        w.on_message_received(&msg(0.0, 0x123, &[0xDE, 0xAD, 0xBE, 0xEF]))
            .unwrap();
        w.on_message_received(&msg(0.005, 0x7FF, &[0x11])).unwrap();
        w.finish().unwrap();
    }

    let cols = ingest::parse(&path).expect("ingest asc");
    assert_eq!(cols.len(), 2, "frame count");
    assert_eq!(cols.can_id, vec![0x123, 0x7FF]);
    assert_eq!(&cols.data[0][..4], &[0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(cols.data[1][0], 0x11);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn unsupported_extension_errors() {
    let path = tmp("nope.txt");
    std::fs::write(&path, b"not a log").unwrap();
    assert!(ingest::parse(&path).is_err());
    let _ = std::fs::remove_file(&path);
}

//! Headless ingest smoke tool: parse a BLF/ASC log and print a summary.
//!
//! `cargo run -p slipstream-core --example dump -- path/to/log.blf [N]`
//! (N = number of head rows to print, default 5)

use std::path::Path;

use slipstream_core::ingest;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = match args.next() {
        Some(p) => p,
        None => {
            eprintln!("usage: dump <log.blf|log.asc> [head_rows]");
            std::process::exit(2);
        }
    };
    let head: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);

    match ingest::parse(Path::new(&path)) {
        Ok(cols) => {
            let n = cols.len();
            println!("{path}: {n} frames");
            if n > 0 {
                println!(
                    "  time span: {:.6}..{:.6} s",
                    cols.timestamp[0],
                    cols.timestamp[n - 1]
                );
            }
            for i in 0..n.min(head) {
                let dlc = cols.dlc[i] as usize;
                let bytes: Vec<String> =
                    cols.data[i][..dlc].iter().map(|b| format!("{b:02X}")).collect();
                println!(
                    "  t={:.6} ch={} id=0x{:X} fd={} dlc={} [{}]",
                    cols.timestamp[i],
                    cols.channel[i],
                    cols.can_id[i],
                    cols.is_fd[i],
                    dlc,
                    bytes.join(" ")
                );
            }
        }
        Err(e) => {
            eprintln!("{path}: ERROR {e}");
            std::process::exit(1);
        }
    }
}

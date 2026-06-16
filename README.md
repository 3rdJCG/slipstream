# slipstream

High-performance CAN log analyzer — fast analysis of multi-GB **BLF/ASC** logs, **DBC** signal decoding, and (later) **ODX/UDS** diagnostic visualization.

A Rust rewrite of an earlier Python tool that was too slow on large logs. The
speed comes from the **core** (compiled bulk parse → columnar store → vectorized
decode → decimation), *not* the GUI.

## Workspace layout

```
crates/
  core/        slipstream-core  — headless engine (NO GUI deps)
    ingest/    BLF + ASC parsers (→ columnar FrameColumns)
    store      columnar store + (planned) Parquet cache; serves row windows
    dbc        DBC database + vectorized signal decode
    query      view-driven query API (Session): decimate / rows / stats
  gui-egui/    slipstream-gui-egui — thin egui view (binary: `slipstream`)
```

### The egui → Tauri migration boundary

egui is the **dev/validation UI**: fastest path to working software and to
locking the core API against real data. A **Tauri/web** front-end will follow for
polish — and it reuses `core` unchanged, wrapping the same query methods as
`#[tauri::command]`. The query types in `core::query` are already `serde`-(de)serializable,
so they double as the IPC contract.

Three rules keep that migration cheap:
1. `core` never depends on egui (no GUI types leak in).
2. The `core` public API is RPC-shaped: take a serde request, return a serde value.
3. Loaded session/state lives in `core::Session`, not in widgets.

Every query is sized to the **view** (a decimated series ≈ screen width, a window
of rows) — the full multi-GB dataset never crosses the boundary.

## Run

```sh
cargo run -p slipstream-gui-egui   # opens the egui app with demo data
```

## Roadmap

- **P0 — Ingest core**: real BLF/ASC parsers (mmap + rayon; zlib container decompress for BLF), Parquet cache. Target: multi-GB in seconds.
- **P1 — Analysis UI**: DBC decode + time-series plot, search/filter/statistics, log compare/diff.
- **P2 — Diagnostics**: ISO-TP reassembly → UDS decode → ODX naming.

Current state: scaffold with a headless core API and an egui skeleton (signal
tree / decimated plot / virtualized frame table / stats) driven by **synthetic
demo data**. Parsers and DBC decode are stubbed (`TODO(P0)` / `TODO(P1)`).

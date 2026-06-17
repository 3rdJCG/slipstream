# CLAUDE.md — slipstream（CANログ解析ツール）

高性能なCANログ解析ツール。現状は Rust コア + egui GUI、**将来的に Tauri / Web
フロントエンド**へ移行する前提（egui→Tauri 移行は明示的な前提条件）。マルチGB
のログに対して遅すぎた旧 Python ツールの置き換え。

このファイルは**生きたロードマップ**である。チェックリストを上から順に進めること。
作業の途中で新しい機能やサブタスクが出てきたら、ここに追記する（適切なフェーズ、
または「Backlog」の下に）。完了したらチェックを入れる。

## 作業上の取り決め（Working agreement）

- 自律的に進めること。各ステップごとの指示を待たない。
- 意味のある変更を加えるたびに、`cargo build` と `cargo test` をグリーンに保つこと。
- `core` はヘッドレスに保つこと。egui/Tauri に決して依存させない。下記の3ルールを参照。
- 周囲のコードのスタイルに合わせて編集すること。

## アーキテクチャ（これらは壊さないこと）

```
crates/core/      slipstream-core — ヘッドレスなエンジン（GUI 依存なし）
  ingest/         BLF パース（`blf_asc` crate 経由）+ ASC パース（自前 tolerant パーサ） -> FrameColumns
  store           列指向ストア（SoA） + 行ウィンドウ + （計画中）Parquet キャッシュ
  dbc             DBC データベース + ベクトル化シグナルデコード
  query           ビュー駆動のクエリ API（Session）: decimate / rows / stats / diff
crates/gui-egui/  slipstream-gui-egui — 薄い egui ビュー（バイナリ: `slipstream`）
```

**egui→Tauri 移行を安価に保つ3つのルール:**
1. `core` に GUI 型を持ち込まない。
2. `core` の公開 API は RPC 形式: serde リクエストを受け取り、serde 値を返す。
3. ロード済みのセッション/状態は、ウィジェットではなく `core::Session` に置く。

すべてのクエリは**ビュー**のサイズに合わせる（間引き済み系列 ≈ 画面幅、行の
ウィンドウ）— データセット全体が境界を越えることは決してない。

## ビルド / 実行 / テスト

```sh
cargo build
cargo test
cargo run -p slipstream-gui-egui              # デモデータ
cargo run -p slipstream-gui-egui -- file.blf  # 実際のログを開く
```

---

## 機能（Features）

### P0 — インジェストのコア
- [x] BLF パース → `FrameColumns`（`blf_asc::BlfReader` 経由）
- [x] ASC パース → `FrameColumns`（`blf_asc::AscReader` 経由）
- [x] フォーマット判定 + `Session::open(path)`
- [x] gui-egui でファイルを開く CLI 引数（デモへのフォールバック）
- [x] ラウンドトリップのインジェストテスト（writer → ingest）
- [x] **ASC を自前の tolerant パーサに置換** — `blf_asc` の ASC reader は実ログの `Statistic:` / `Status:` / `J1939TP` / 名前付き `CANFD` / `ErrorFrame` 行で hard error し、以後の行を全部失う（`samples/logfile.asc` で確認）。フレーム行以外はスキップして読み続ける自前パーサにする。BLF は `blf_asc` のままで実ファイル検証OK
- [ ] BLF コンテナの解凍を並列化（rayon）— *最適化。ベンチマークには実際のマルチGBサンプルが必要*
- [ ] 初回インジェスト時の Parquet（または列指向）キャッシュ → 再オープンを即時化
- [ ] 非常に大きなファイル向けの mmap ベース読み込み
- [ ] インジェスト進捗の UI への報告（channel/callback）

### P1 — 解析 UI
- [x] DBC ロード（`can-dbc` crate）→ `DbcDatabase`
- [x] ベクトル化シグナルデコード（列単位: start bit / 長さ / エンディアン / 符号付き / scale+offset）。マルチプレクスは未対応（下記 Backlog）
- [x] デコード済みシグナルの時系列プロット（デモ系列を実データに置き換え。demo は encode→decode を実走）
- [x] 検索 / フィルタ（id / 時間範囲 / channel による。core: `FrameFilter` + `filtered_rows`/`filtered_count`。signal 値での絞り込みは下記「述語エンジン」で対応）
- [x] 統計（件数、min/max、平均、周期/周波数）
- [x] メッセージ単位の統計（ID ごとの件数・実測周期。DBC 無し健全性の周期推定に流用）
- [x] バス負荷率（bus load %。channel ごと、時間窓で算出）— gui(Analysis): 「Bus load」折りたたみセクション（bitrate/window の DragValue + channel/t_start/t_end/load% テーブル、500 行で打ち切り）
- [x] エクスポート（フィルタ済み行 / デコード済みシグナル / 健全性レポートを出力。まず CSV、のち Parquet）— core: `Session::export_frames_csv`/`export_signal_csv`/`export_health_csv`（`export.rs`、`BufWriter<File>`、書き込み行数を返す）。Parquet は今後
- [x] ログ比較 / diff（2つのログを並べて表示。回帰/異常ビュー）
- [x] CLI のみではなくファイルを開くダイアログ（`rfd`）
- [ ] 複数シグナルのプロット軸 / カーソル / 範囲ズーム — カーソル読み出し・範囲ズーム（ズームに追従して再間引き、1フレーム遅延）・正規化トグル（各シグナルを自身の min/max で 0..1 にスケール。真のデュアル Y 軸の代用）は実装済み。真のマルチ Y 軸は egui_plot 非対応のため今後

### P1 — Session / DBC 管理（状態の可変化）
タブ UI は同一 `core::Session` を共有するため、`open` 後に状態を変えられる経路が要る
（現状の `Session` は構築時固定）。
- [x] `Session` を可変化：`open` 後に DBC を後付けロード / 差し替え（`load_dbc` / `set_dbc`）
- [ ] 複数 DBC + **channel→DBC マッピング**（channel/bus ごとに別の DBC を適用。`DbcDatabase` の単一前提を見直す）
- [ ] シグナル同定を `Message.Signal` で修飾（同名衝突を回避。`find_signal(name)` のグローバル名引きを置換）
- [x] Config タブでのロード/設定変更を Analysis/Graph に即時反映する経路

### P1 — ロード済みファイル管理 / チャンネル操作
複数の LOG / DBC を同時にロードし、一覧表示・個別削除できるようにする。チャンネル
（bus）を意識した運用にする。
- [x] core: `Session` を複数 LOG 保持に（`logs: Vec<LoadedLog>`、`add_log`/`remove_log`/`list_logs`）。全 LOG を結合した派生ストアを再構築（既存クエリは結合ストアをそのまま読む。複数時は timestamp 昇順マージ）
- [x] core: 複数 DBC 保持（`dbcs: Vec<LoadedDbc{ path, channel: Option<u8>, db }>`、`add_dbc`/`remove_dbc`/`set_dbc_channel`/`list_dbcs`）。`channel=None` は全チャンネル適用
- [x] core: **チャンネル別デコード** — フレームは「その channel に割り当てられた（or 全体）DBC」でデコード。`available_signals`/`signal_series` を channel 対応に（`SignalMeta` に channel 付与、`find_signal` は全 DBC 横断）
- [x] core: チャンネル一覧 `Session::channels()`（全 LOG distinct channel）。※ channel 別フレーム数は今後
- [x] gui(Config): ロード済み **LOG 一覧**（path / frames / channels）+ 個別削除ボタン、追加は既存 Open log を addtive に
- [x] gui(Config): ロード済み **DBC 一覧**（path / messages / 割り当て channel）+ 個別削除 + channel 割り当て（All / 特定 ch）の編集
- [x] gui: チャンネル別操作（channel の表示 ON/OFF フィルタ、channel→DBC 割り当て UI）
- [x] 旧 `load_log`/`load_dbc`（置き換え型）は addtive 版へ移行（互換のため当面 clear+add で温存可）

### P1 — シグナル述語エンジン（検索・フィルタ・ゲート共通）
値の閾値・比較を時間軸上で評価する述語型を `core` に1つ用意し、検索/フィルタ・
健全性のゲート条件・統計区間の指定で再利用する（バラバラに作らない）。
- [x] 時間軸シグナル述語型（`Signal 比較 値`）を `core` に定義（`predicate::Predicate` + `PredEval::is_active(t)`、値はサンプル間で前方保持）
- [x] **複数条件を AND/OR で組み合わせ**可能にする（`Predicate::All`/`Any`/`Not`）
- [x] `FrameFilter`（id/channel/time）を拡張し、この述語で signal 値フィルタを実現（`FrameFilter.predicate`、`matching_indices` で評価）
- [x] 同じ述語を健全性のゲート条件に使う（`HealthRule.gate: Predicate`、`build_pred` 共用）。統計の対象区間指定への適用は今後

### P1 — フレーム健全性（周期/存在チェック）
フレームが想定どおりに来ているかを検査する。判定は**フレーム単位（存在・周期）**で、
ペイロードの意味は解釈しない（ただし下記ゲート条件のみシグナル値を読む）。ルール設定
も結果表示も **Analysis タブで完結**する。DBC で周期が規定されているフレームはその
周期を基準に、規定がない（または DBC 自体が無い）場合は手動で条件を追加できる。
- [ ] 周期判定は **`(can_id, channel)` 単位**で行う
- [x] DBC の周期属性（`GenMsgCycleTime` 等）を読み、フレームごとの期待周期を取得（`DbcMessage.expected_cycle_ms` / `Session::dbc_health_rules`）
- [x] 実測周期の検査（違反区間を列挙。core: `health::scan_cadence` + `Session::check_health` → Missing=gap過大 / Excessive=gap過小 / NoData。欠落と遅延の細分はまだ）
- [ ] 許容差は**絶対値（ms）と百分率（%）の両方**で指定可能にする
- [ ] DBC 未定義フレーム向けの手動ルール追加（対象 ID/名・期待周期・許容差を UI で設定）
- [ ] **ゲート条件**：チェックを有効化する前提条件（上記「述語エンジン」を使用）。
      例：電源を表すシグナル値が ON の区間だけ周期チェックする等。複数条件を AND/OR で組める
- [ ] ゲート信号のチャタリング対策（ヒステリシス / デバウンス）
- [ ] ログ端のグレース（最初の出現前・最後の出現後は欠落と見なさない）
- [ ] DBC・手動ルールいずれにも該当しない ID を「未知フレーム」として一覧
- [x] 結果スキーマ：RPC 形式の `HealthReport` 型（違反フレーム・違反区間・周期統計）を定義
- [x] チェック結果を Analysis タブに表示（違反一覧 + 違反区間。行テーブルと連動）
- [x] 健全性ルールセットの保存 / 読み込み（core: `health::HealthRuleSet::save/load`、JSON。手動ルール + ゲートを 1 セットで永続化）
      - メモ: ゲートは述語エンジン（`predicate::Predicate`、AND/OR/Not 対応）に統合済み。ヒステリシス・`(can_id,channel)` 粒度・`HealthReport` 型・ゲート構築 UI は上記未チェック項目で対応予定

### P1 — 画面構成（タブ UI シェル）
タブで画面を切り替える構成にする。各タブは同一の `core::Session` を共有し、タブ
自体は egui 側の薄いビュー（状態は `core` に置く 3 ルールを守る）。タブ名は仮称、
適宜変更してよい。
- [x] タブ切り替えのシェル（トップレベルのタブバー + 各タブのビュー）
- [x] **Config** タブ — BLF/ASC ファイルの読み込み、DBC の設定、インジェスト進捗の表示
- [x] **Analysis** タブ — フレーム単位の解析（行テーブル / 検索・フィルタ / 統計 / 健全性のルール設定＋結果）
- [x] **Graph** タブ — デコード済みシグナルのプロット（カーソル読み出し / 範囲ズーム再間引き / 正規化トグル。真のマルチ Y 軸は今後）
- [x] **Diff** タブ — 2つのログを CAN id 単位で比較（`Session::diff_logs`、存在差分・件数・Δ件数・平均周期のテーブル表示。OnlyA/OnlyB を色分け）
- [ ] タブ間で選択状態（時間範囲・対象 ID/シグナル）を共有・連動させる
- [ ] 各タブの空状態（log 未ロード / DBC 未設定）の表示

### P1 — プロジェクト / 永続化
- [ ] プロジェクト（ワークスペース）の保存・再オープン：log パス + 複数 DBC + channel マッピング
      + 健全性ルール + ビュー状態を 1 セットとして永続化
- [ ] エラー / 部分破損の UI 表面化（パース失敗・破損フレームの通知。エラーフレーム保持は下記 Backlog）

### P2 — 診断（ODX/UDS）
- [ ] ISO-TP（ISO 15765-2）によるマルチフレームメッセージの再構成
- [ ] UDS（ISO 14229）サービスのリクエスト/レスポンス解釈
- [ ] ODX（ISO 22901 / PDX zip）パースによる命名（services, DIDs, DTCs）
- [ ] 診断シーケンスの可視化（リクエスト↔レスポンスの対応付け）
- [ ] ODX のスコープ絞り込み（フルスペックは巨大 — 使える範囲を決める）

### Backlog / 後で見つかったもの
- [ ] モデルでは channel 列が `u8` だが `blf_asc::Message.channel` は `u16` — 255 を超える channel が問題になる場合は見直す
- [ ] エラーフレーム / 方向（Rx/Tx）を列として保持する（現状は破棄している）
- [ ] マルチプレクスシグナルのデコード（マルチプレクサ選択子を尊重する。現状は無条件デコード）
- [ ] DBC の文字コード（現状 UTF-8 前提。実ファイルは CP1252 等もあり得る → `can_dbc::encodings` で対応可）
- [ ] フィルタが毎回 O(n) 全スキャン（`matching_indices`）— マルチGB + 対話フィルタ向けにインデックス化 / フィルタ結果キャッシュ
- [x] 拡張 ID（29bit）と標準 ID の区別フラグ（現状 `can_id: u32` のみで判別不可）

## メモ / 決定事項（Notes / decisions）
- `blf_asc` はシングルスレッドのイテレータ（コンテナを逐次 inflate）。正しく動作し、
  旧 Python よりはるかに高速だが、上記の並列解凍の最適化が「マルチGB を数秒で」への
  道筋となる。最適化の前に、実際の Vector `.blf` サンプルで検証すること。
- 実サンプル検証（`python-can` テストデータ、`samples/` に取得・gitignore）:
  BLF は CAN/CAN_MESSAGE2/CAN-FD/FD64 すべて正しくパース。ASC は `blf_asc` が
  実 `logfile.asc`（Statistic/Status/J1939TP/名前付きCANFD 行）で hard error →
  上記 P0「ASC 自前パーサ」で対応。ヘッドレス検証ツール: `cargo run -p slipstream-core
  --example dump -- <log> [N]`。

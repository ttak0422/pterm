# Proposal 0001: プロトコルバージョンハンドシェイク (HELLO)

Status: Implemented (2026-07-05)
Depends on: なし（他 proposal の前提）

## 背景

pterm のワイヤプロトコルにはバージョン交渉がない。デーモンは Neovim を越えて生き続けるため、
「旧バイナリのデーモン × 新バイナリのクライアント」は運用上の常態であり、
非互換なプロトコル変更を入れた瞬間に無言で壊れる。
herdr は `Hello` でバージョンを交換し、不一致を明示エラーにしている（`PROTOCOL_VERSION` 方式）。

現行実装の好都合な性質: サーバは未知のメッセージ型を warn ログして無視し
（`server.rs` `process_client_recv_buf` の `_` アーム）、ブリッジも未知の型を無視する
（`bridge.rs` の `_ => {}`）。したがって HELLO の追加は前方・後方互換。

## ゴール

- クライアント・デーモン双方が相手のプロトコルバージョンを知り、乖離を検知・通知できる。
- 旧デーモン／旧クライアントと混在しても現行動作を壊さない。
- 後続 proposal（履歴 replay の要求フラグ、handoff のバージョン判定）の土台を提供する。

## 非ゴール

- 不一致時の自動アップグレード（Proposal 0003 handoff が担う）。
- 複数バージョンの同時サポート（major 不一致は「警告して現状動作」まで）。

## プロトコル変更

`proto/src/lib.rs`:

```rust
/// Wire protocol version. Bump on incompatible changes.
pub const PROTO_VERSION: u32 = 1;

pub mod client {
    /// Handshake. Payload: [proto_version: u32 LE] [flags: u32 LE]
    pub const HELLO: u8 = 0x07;
}

pub mod hello_flags {
    /// Client wants scrollback history replay on attach (Proposal 0002).
    pub const REQUEST_HISTORY: u32 = 1 << 0;
}

pub mod server {
    /// Handshake reply. Payload:
    /// [proto_version: u32 LE] [pkg_version: UTF-8 (残り全部)]
    pub const HELLO_ACK: u8 = 0x83;
}
```

- `pkg_version` は `env!("CARGO_PKG_VERSION")`。handoff 時の「デーモンが自分より古いか」の判定
  と診断（dump への転載）に使う。
- encode/parse ヘルパ（`encode_hello` / `parse_hello` / `encode_hello_ack` / `parse_hello_ack`）
  を既存の resize/exit ヘルパと同じ流儀で追加。長さ不正は `DecodeError` に variant 追加。

## シーケンス

```text
bridge                     daemon
  │ HELLO(v, flags)          │   ← RESIZE と同じ write_all にまとめて送る（順序保証）
  │ RESIZE(cols, rows)       │
  │                          │ HELLO 受信: client.proto = v, client.flags = flags
  │            HELLO_ACK(v') │   ← 最初の STATE_SYNC より前に必ずキューする
  │            STATE_SYNC    │
```

- **HELLO を送らないクライアント**（旧ブリッジ、redraw 等の短命クライアント）は `proto = 0`
  として扱い、現行動作のまま。
- **HELLO_ACK が来ないデーモン**（旧デーモン）: ブリッジは「最初の STATE_SYNC 到着までに
  ACK なし」で proto 0 と判定する。タイムアウト不要（STATE_SYNC をアンカーにする）。

## 不一致時の動作（v1 ポリシー）

- ブリッジ: `daemon_proto != PROTO_VERSION`（0 含む）のとき、stderr にログしつつ、
  端末に 1 行通知を書いてから継続する:
  `[pterm: daemon protocol vN / client vM — restart the session or run pterm upgrade]`
  （通知は STATE_SYNC 適用の前に stdout へ書く。画面再描画で消えても Neovim の
  スクロールバックには残る）
- デーモン: proto 0 クライアントは従来どおり。未知の flags ビットは無視。
- 動作を止めない。v1 時点で既知の非互換は存在しないため「検知と通知」まで。

## 実装ポイント

- `src/server.rs`: `Client` に `proto: u32` と `flags: u32`（default 0）を追加。
  `process_client_recv_buf` に `HELLO` アームを追加し、`HELLO_ACK` を send_buf に積む。
  注意: RESIZE の `replace_send_buf=true` が同一バッチ内で ACK を消さないよう、
  HELLO 処理は同フレームバッチ内の RESIZE より先に完了している（decode_frames は順序保存、
  ブリッジは HELLO→RESIZE の順で送る）。ただし**別クライアント**の RESIZE が
  全クライアントの send_buf を差し替える経路があるため、`send_snapshot_to_client` の
  `replace_send_buf` で ACK ごと消える競合がある。対策: ACK 未送信フラグを `Client` に持ち、
  snapshot 送信時に ACK が先頭に来るよう再エンキューする。
- `src/bridge.rs`: 接続直後に HELLO + RESIZE を 1 回の `write_all` で送る。
  受信ループに `HELLO_ACK` アームを追加。
- 短命クライアント（redraw/dump/snapshot-text）: HELLO 不要（proto 0 のまま）。
  将来必要になったら送る。

## テスト

- proto: hello/hello_ack roundtrip、不正長で DecodeError。
- server: HELLO→RESIZE を送ると HELLO_ACK が STATE_SYNC より前に届く。
  HELLO なしクライアントが従来どおり動く（回帰）。
  他クライアントの RESIZE 起因 replace_send_buf で ACK が失われない。
- bridge: 定数系のユニットテスト（既存の cleanup テストと同じ粒度）。

## 未決事項

- なし。ポリシー変更（不一致で拒否等）は非互換が実際に生まれたときに再検討。

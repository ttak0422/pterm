# Proposal 0002: アタッチ時のスクロールバック replay (HISTORY)

Status: Implemented (2026-07-05)
Depends on: 0001 (HELLO の REQUEST_HISTORY フラグ)

## 背景

デーモン内の vt100 パーサは 10,000 行のスクロールバックを保持しているが、
アタッチ時の `STATE_SYNC` は `state_formatted()`（`session.rs` `build_snapshot`）による
**現在の可視画面のみ**の再構築で、履歴を replay しない。
そのため Neovim 再起動後の再アタッチでは terminal buffer に履歴が入らず、
検索・marker・ノーマルモード移動が過去出力に効かない。

Neovim terminal 操作の利点は「buffer にバイトが流れて蓄積される」ことから来るので、
履歴をバイト列として一度流し込めば復元できる。サーバ完全エミュレーション
（herdr 型フレーム配信）に寄せる必要はない。

## ゴール

- 再アタッチ時、デーモンが保持するスクロールバックを Neovim の terminal buffer に復元する。
- REDRAW / RESIZE の再 snapshot では履歴を重複送信しない（auto_redraw は BufEnter ごとに
  走るため、ここを間違えると履歴が無限に増殖する）。
- 旧クライアント・旧デーモンとの混在で壊れない。

## 非ゴール

- alternate screen アプリの履歴復元（v1 では altscreen 中は履歴 replay をスキップ。
  vt100 の scrollback は main screen のもので、altscreen 表示中の抽出 API 整合を
  確認してから対応する）。
- リブート越えの永続化（Proposal 0004）。

## プロトコル変更

- `server::HISTORY = 0x84`。Payload: 生エスケープシーケンス列（OUTPUT と同じ扱いで
  stdout に書けるバイト列）。
- クライアントは HELLO の `REQUEST_HISTORY` フラグで要求する。フラグなし／HELLO なし
  クライアントには送らない → 後方互換は自動的に成立。

## 履歴バイト列の構築 (`Session::history_formatted`)

vt100 0.16 の API で実現できることは確認済み:
`Screen::set_scrollback(usize)`（&mut、最大値に clamp）/ `scrollback()` /
`rows_formatted(start, width)` / `row_wrapped(row)`。

```text
fn history_formatted(&mut self, max_lines: usize) -> Vec<u8>
```

1. `alternate_screen()` なら空を返す（v1 制限）。
2. 現在の offset を退避 → `set_scrollback(usize::MAX)` で最古へ。
3. 可視ウィンドウ（rows 行）ずつ `rows_formatted(0, cols)` で取り出し、offset を
   rows ずつ下げながら最古→最新の順に emit。最後のウィンドウは重複行が出るので
   「emit 済み行数」を追跡してスキップする。
4. 各行: 行バイト + `\x1b[0m` + (`row_wrapped` でなければ `\r\n`)。
   wrapped 行に改行を入れないことで、折り返された長い行が buffer 上で 1 行に再結合され、
   検索が効くようになる。
5. offset を 0 に復元（退避値ではなく 0 で良い。デーモン側 offset は常に 0 運用）。
6. 末尾に viewport 押し出し用のパディング `\r\n` × rows を付ける。
   これで履歴全体がクライアント viewport の上（= scrollback）に押し出され、
   直後の STATE_SYNC の絶対位置再描画が履歴末尾を上書きしない。

上限: `max_lines` は env `PTERM_HISTORY_REPLAY_LINES`（default: 無制限 = パーサの
スクロールバック容量が実質上限）。サイズの実質上限は 10,000 行 × 幅 × SGR で数 MB 程度。
既存の SendQueue バックプレッシャ機構（`WouldBlock` で writable 待ち）がそのまま吸収する。

注意: `set_scrollback` は `&mut Screen` を要するため、`Session::snapshot()` と違い
`&mut self` になる。呼び出し側（server.rs）は既に `&mut self.session` を持っているので影響なし。

## サーバ側の配送ルール

- `Client` に `wants_history: bool`（HELLO フラグから設定）を追加。
- `send_snapshot_to_client` で `pending_snapshot == true` の初回送信時のみ、
  STATE_SYNC の**直前**に HISTORY フレームを積み、`wants_history = false` に落とす。
- REDRAW / 他クライアント起因 RESIZE の再 snapshot は `pending_snapshot == false`
  なので履歴は流れない。既存の「初回 snapshot は RESIZE か初 OUTPUT で送る」規則に
  そのまま乗る。
- `replace_send_buf` で HISTORY が消えるケース: 初回 RESIZE 時は HISTORY+STATE_SYNC を
  同時に積むので問題なし。積んだ後に別クライアントの RESIZE が来た場合、
  `wants_history` は既に false — このとき履歴未達のまま画面だけ更新される稀ケースを許容する
  （再現条件が「アタッチと同時に別クライアントがリサイズ」で、失うのは履歴のみ）。

## ブリッジ側

- `HISTORY` アームを追加し、payload を `output_batch` に積むだけ（OUTPUT と同じ経路）。
  STATE_SYNC 前のキーボードクリーンアップ注入は STATE_SYNC 側の既存処理に任せる。
- HELLO に `REQUEST_HISTORY` を立てるのは**初回接続のみ**。
  再接続（Proposal 0003 の RECONNECT）ではフラグを立てず、履歴の重複を防ぐ。

## Neovim プラグイン

変更不要。バイトが流れてくるだけで、terminal buffer が履歴として蓄積する。

## テスト

- session: rows を超える行数を流す → `history_formatted` に最古行が含まれ、
  可視画面の行は含まれない（重複なし）。offset が 0 に戻る。
  wrapped 行に `\r\n` が入らない。altscreen 中は空。max_lines が効く。
- server: HELLO(REQUEST_HISTORY)+RESIZE → HISTORY が STATE_SYNC の直前に 1 回だけ届く。
  その後の REDRAW / RESIZE で HISTORY が再送されない。フラグなしクライアントには届かない。

## 未決事項

- リサイズ後の履歴行の再折り返し: vt100 は scrollback を rewrap しない前提で設計する。
  旧幅で記録された行はそのまま流れる（実害は表示幅の不一致のみ）。実装時に挙動を確認し、
  必要なら doc コメントに明記する。

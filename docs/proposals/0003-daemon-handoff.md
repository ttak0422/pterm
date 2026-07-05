# Proposal 0003: デーモン handoff（無停止バイナリ更新）

Status: Withdrawn (2026-07-05) — 無停止バイナリ更新は対応不要と判断。バージョン乖離の検知・通知は 0001 で対応済みで、更新は `pterm kill` + 再作成で足りる。
Depends on: 0001 (バージョン検知), 0002 (replay バイト列を状態シリアライズとして再利用)

## 背景

デーモンは Neovim より長生きするため、pterm を更新しても旧バイナリのデーモンが残り続ける。
現状の解消手段は `pterm kill`（= 子プロセスごとセッション消滅）しかない。
herdr は PTY マスタ fd を SCM_RIGHTS で新サーバへ渡す handoff プロトコルで
無停止更新を実現しており、この方式は pterm にそのまま適用できる。

pterm 固有の好条件: **VT 状態のシリアライズ形式を新規に設計する必要がない**。
`build_snapshot()` + `history_formatted()`（0002）の出力バイト列を新デーモンの
vt100 パーサに食わせれば、画面・スクロールバック・SessionCallbacks の追跡状態
（kitty keyboard、title stack、passthrough キュー等）が同じ経路で再構築される。
snapshot 形式がそのまま状態転送形式になる。

## ゴール

- 子プロセス（シェル/TUI）と Neovim 側バッファを生かしたまま、デーモンプロセスだけを
  新バイナリに差し替える。
- 失敗時は旧デーモンがそのまま継続する（handoff は常に中断安全）。

## 非ゴール

- Neovim バッファの無瞬断（再アタッチによる画面再描画 1 回は許容）。
- 自動アップグレード（トリガは明示的な `pterm upgrade`。自動化は Lua 側で将来検討）。

## CLI

```sh
pterm upgrade <session>    # このバイナリで該当セッションのデーモンを置き換える
pterm upgrade --all        # list を回して全セッションに適用
```

親プロセスは結果 JSON を出力、実作業は cmd_new と同じ fork+setsid した子（候補デーモン）が行う。

## プロトコル変更

- `client::TAKEOVER = 0x08`。Payload: `[proto_version: u32 LE]`。候補デーモンが旧デーモンの
  セッションソケットに通常クライアントとして接続して送る。
- `client::TAKEOVER_ACK = 0x09`。Payload なし。候補デーモンが引き継ぎ完了を通知する。
- `server::HANDOFF = 0x85`。Payload: 状態 JSON。**同じ sendmsg の SCM_RIGHTS で
  PTY マスタ fd を添付する**（`nix::sys::socket::sendmsg` + `ControlMessage::ScmRights`）。
- `server::RECONNECT = 0x86`。Payload なし。ブリッジへ「再接続せよ」を通知する。

状態 JSON（`schema_version: 1`）:

```json
{
  "session": "name",
  "child_pid": 12345,
  "cols": 120, "rows": 40,
  "exited": null,
  "replay_b64": "<history_formatted + build_snapshot を base64>"
}
```

replay_b64 に含まれない揮発状態（pending DA カウンタ、output_filter の途中状態、
pending_pty_output）は handoff 前に drain/flush して空にしてから送る。

## シーケンス

```text
候補デーモン (新バイナリ)                旧デーモン
  │ connect + TAKEOVER(v) ──────────────→ │ HELLO_ACK で互いのバージョン確認済み前提
  │                                       │ 1. PTY drain + 全クライアント flush
  │                                       │ 2. PTY fd を poll から deregister（読取停止）
  │ ←────────── HANDOFF(state JSON) + fd  │ 3. 状態送信、TakeoverPending(deadline=5s) へ
  │ 4. Session::from_handoff:             │
  │    - fd 採用, 新パーサに replay 投入   │
  │ 5. <dir>/socket.new に bind           │
  │ 6. rename(socket.new → socket)        │   ← atomic。旧 listener は旧 inode で生き続ける
  │ 7. TAKEOVER_ACK ─────────────────────→ │ 8. 全ブリッジへ RECONNECT + flush
  │                                       │ 9. listener/fd を閉じて exit
  │ ←── ブリッジが socket へ再接続          │
  │     (HELLO は REQUEST_HISTORY なし)     │
```

- 手順 6 の rename が肝: bind 済み Unix socket のパスは rename しても既存接続に影響しない。
  旧デーモンのソケット生存チェック（`run()` 先頭の `is_socket()` 確認）は新ソケットでも
  socket 判定が通るため誤爆しない。旧デーモンは ACK 受信を契機に**明示的に** exit する。
- 手順 8 のブリッジ再接続: RECONNECT 受信 → 旧接続 close → socket パスへ 50ms 間隔で
  最大 3s リトライ（`wait_for_socket` と同じ定数）→ HELLO(履歴フラグなし) + RESIZE。
  画面は STATE_SYNC で再描画され、履歴は重複しない。
- 旧ブリッジ（RECONNECT を知らない）: メッセージを無視し、旧デーモン exit の EOF で
  ブリッジ終了 = 従来の切断と同じ挙動に落ちる。互換性は「劣化はするが壊れない」。

## 失敗時の巻き戻し

- 旧デーモン側: `TakeoverPending` 状態で deadline (5s) 内に ACK が来なければ、
  PTY fd を poll に再登録して通常運転に復帰、warn ログ。TAKEOVER の多重受付は拒否
  （Pending 中の新規 TAKEOVER は無視）。
- 候補デーモン側: どこで失敗しても自分が exit するだけ。socket.new は unlink して終了。
- `ponytail:` 巻き戻しは「PTY 再登録」のみで済む。handoff 中に旧デーモンが受けた INPUT は
  通常どおり PTY に書かれ続ける（INPUT 経路は止めない。止めるのは読取だけ）。

## 子プロセス監視の制約（既知の上限）

旧デーモンの exit で子プロセスは init/launchd に reparent される。
新デーモンは `waitpid(child_pid)` できない（ECHILD）。対応:

- `check_exit` に handoff 由来フラグを持たせ、waitpid の代わりに
  `kill(pid, 0) == ESRCH` と PTY read の EOF/EIO で終了検知する。
- **exit code は取得不能**。EXIT payload は 0 で送る（Neovim には
  `[Process exited 0]` と出る）。これは Unix の制約で herdr 型 handoff にも共通する上限。
  ドキュメント（DESIGN.md）に明記する。

## 実装ポイント

- `proto/`: 上記 4 メッセージ + 状態 JSON の serde 型（`pterm-proto` は依存ゼロを保つため、
  JSON 型は `src/` 側 = `src/handoff.rs` に置き、proto には型定数のみ追加する）。
- `src/session.rs`: `Session::from_handoff(name, fd, child_pid, cols, rows, replay: &[u8])`。
  fd の non-blocking 設定を確認、`Pty` を fd から構築するコンストラクタを `pty.rs` に追加。
- `src/server.rs`: TAKEOVER/TAKEOVER_ACK アーム、`TakeoverPending` 状態、RECONNECT 配送。
- `src/bridge.rs`: RECONNECT アームと再接続ループ。再接続をまたいで RawModeGuard と
  SIGWINCH 配線は維持されるので、ソケットの繋ぎ直しと poll 再登録だけで済む。
- `src/main.rs`: `cmd_upgrade`。

## テスト

- 状態 JSON roundtrip（serde）。
- 統合寄りテスト（既存の server.rs:711 の流儀）: 実セッションを spawn → 同一プロセス内で
  候補側ロジックを実行 → handoff 後に (a) child_pid が同一、(b) 旧デーモンループが終了、
  (c) 新 Session の snapshot に handoff 前の画面内容が含まれる、(d) PTY へ書いた出力が
  新デーモン経由でクライアントに届く、を検証。
- ブリッジ RECONNECT: フレームを直接食わせて再接続パスに入ることをユニットで確認。

## 未決事項

- macOS の launchd 配下での reparent 挙動（ESRCH 検知が遅延する可能性）。実装時に
  手元検証し、必要なら PTY EOF 検知を主、kill(0) を従にする。

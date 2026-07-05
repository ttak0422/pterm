# Proposal 0004: リブート越えセッション復元

Status: Withdrawn (2026-07-05) — 対応不要と判断。リブートでプロセスは死ぬため、履歴のみの復元に見合う価値がないと結論。
Depends on: 0002 (replay バイト列をディスク保存形式として再利用)

## 背景

DESIGN.md の TODO「persistence across reboot (save/restore scrollback)」。
ソケットルートは `XDG_RUNTIME_DIR` / `/tmp` 配下でリブートで消える（それが正しい）。
herdr は「プロセスは残らないが状態は snapshot で残す」方式（session.json +
画面履歴の別ファイル）を採っており、この分離が参考になる。

pterm では 0002 の replay バイト列（history + snapshot）がそのまま保存形式になる:
復元時に新パーサへ食わせれば、旧セッションの画面と履歴が「死んだ履歴」として
スクロールバックに復元される。

## ゴール

- リブート（またはデーモン喪失）後、`pterm open <name>` で同名セッションを再作成したとき、
  旧セッションの画面・スクロールバックが terminal buffer の履歴として蘇る。
- 保存はデーモンが自動で行い、ユーザ操作を要しない。

## 非ゴール

- プロセスの復元。子プロセスは死んでいる。復元されるのは出力履歴と cwd のみ。
- **保存していた argv の自動再実行はしない**（v1）。`build` のようなコマンドを
  リブート後に勝手に再実行するのは危険。コマンド解決は通常の open と同じ
  （明示指定 > `$SHELL`）。保存 JSON には argv を記録だけしておき、
  将来 opt-in（`--resume-command` 等）の余地を残す。

## 保存形式

場所: `${XDG_STATE_HOME:-~/.local/state}/pterm/<session>/saved.json`
（階層セッションはソケットと同じくディレクトリ階層で表現）

```json
{
  "schema_version": 1,
  "saved_at": "2026-07-05T12:34:56Z",
  "cwd": "/home/user/project",
  "cmd_args": ["/bin/zsh"],
  "cols": 120, "rows": 40,
  "replay_b64": "<history_formatted + build_snapshot を base64>"
}
```

- replay のサイズ上限はパーサのスクロールバック容量（10,000 行）で自然に決まる。
- `paths.rs` に `state_dir()` / `session_state_path(name)` を追加。
  解決順: `PTERM_STATE_DIR` → `XDG_STATE_HOME/pterm` → `~/.local/state/pterm`。

## 保存タイミング（デーモン側）

- `read_pty` で出力があったら dirty フラグを立てる。
- `run()` ループで `refresh_cwd` と同じスロットル方式により、dirty かつ前回保存から
  60 秒経過で保存（`SAVE_INTERVAL`）。
- 加えて「最後のクライアントの切断時」と「子プロセス exit 時」に即保存。
  リブートは clean shutdown を経ないため、定期保存が本命でイベント保存は補助。
- 書き込みは tmp ファイル + rename のアトミック書き。
- `ponytail:` 保存は同期 write。10k 行 ×SGR で数 MB / 60 秒に 1 回なら
  イベントループの停止は実測で問題にならない見込み。問題になったら
  「保存用スレッドに Vec を投げる」に upgrade する。

## 復元フロー

1. `cmd_open`: ソケットなし & `saved.json` あり → 復元付き create パスへ。
2. `cmd_new` 側に `restore: Option<SavedState>` を渡す:
   - 起動 cwd: ユーザの現在 cwd ではなく `saved.cwd` を使う（存在しなければ現在 cwd に
     フォールバック）。子プロセス spawn 前に `chdir`（`pty.rs` の fork 子側）。
   - `Session::new` 後、spawn した子の出力より**前に** replay バイト列をパーサへ
     `parser.process()` で投入し、続けて区切り行
     `\r\n\x1b[2m--- pterm: restored from <saved_at> ---\x1b[0m\r\n` を投入する。
     これらはパーサにのみ入り（PTY には書かない）、以後のアタッチで
     HISTORY + STATE_SYNC として自然に流れ出る。
3. 復元に使った `saved.json` は残す（次回の定期保存で上書きされる）。
4. `pterm kill` は `saved.json` も削除する（セッションの明示削除 = 状態も削除）。
   ソケット外部削除によるデーモン終了時は削除しない（保存が生き残るのは意図どおり）。

## 復元と 0002 の合成

復元された内容はパーサのスクロールバック上では「画面 rows 行 + 履歴」として
積まれ直すため、復元後の初アタッチでは 0002 の HISTORY 経路がそのまま
旧履歴 + 旧画面 + 区切り行 + 新しいプロンプトを届ける。追加のプロトコル変更は不要。

注意: replay 先頭の `state_formatted()` 由来バイトには入力モード系
（altscreen 突入等）が含まれ得る。保存時に altscreen だったセッションは
0002 と同じ理由で履歴が空になるため、保存する replay は
「history_formatted + **contents 部分のみ**」とし、`input_mode_formatted` 相当
（kitty keyboard 等の SessionCallbacks プレフィクス/サフィクス）は保存しない。
死んだ履歴にキーボードプロトコル状態は不要で、復元後の新プロセスの状態を汚す方が害が大きい。
→ `build_snapshot` を「contents のみ」と「モード込み」に分けるヘルパ分割が前提作業。

## 実装ポイント

- `src/paths.rs`: state dir 解決。
- `src/session.rs`: `build_snapshot` の分割（contents / modes）、
  `Session::preload_replay(&mut self, bytes: &[u8])`。
- `src/server.rs`: dirty フラグ + 定期保存 + イベント保存。
- `src/main.rs`: `cmd_open` の復元分岐、`cmd_kill` の saved.json 削除。
- `src/pty.rs`: spawn 時 cwd 指定。
- Lua / プロトコル: 変更不要。

## テスト

- saved.json の roundtrip とアトミック書き（tmp→rename）。
- session: preload_replay 後の snapshot / history_formatted に旧内容と区切り行が含まれる。
- server: dirty→60s→保存、最終クライアント切断で保存（統合寄りテストの流儀で）。
- kill が saved.json を消す。restore 時に cwd が saved.cwd になる。

## 未決事項

- 保存の暗号化・パーミッション: state ファイルは端末出力の平文を含む。
  `saved.json` は 0600、ディレクトリ 0700 で作成する（実装時に必須とする）。
- 世代管理（直近 N 世代の保持）は必要になるまでやらない。

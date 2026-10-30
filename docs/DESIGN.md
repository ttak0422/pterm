# pterm - Design

## Overview

pterm provides persistent terminal sessions for Neovim.

- `pterm new` starts a background daemon that owns a PTY + child process.
- The Lua plugin uses `pterm open <name> --no-resize` with `jobstart(..., { term = true })`, so create+attach happens in one process without a create/attach race.
- `pterm attach` is a bridge process between Neovim terminal PTY and daemon socket.

The daemon owns terminal state and history; Neovim's libvterm renders the ANSI
views produced from that state. Managed Neovim attachments request canonical
views. Standalone CLI attachments retain the raw-output/snapshot path described
below; “raw” here does not mean byte-for-byte passthrough of all terminal queries.

## Architecture

```text
┌─ Neovim ───────────────────────────────────────────┐
│ Lua plugin                                         │
│ ├─ :Pterm new creates/fetches session              │
│ └─ jobstart(pterm open --no-resize, {term=true})     │
└───────────────────┬────────────────────────────────┘
                    │ terminal PTY (owned by Neovim)
                    ▼
            pterm attach (bridge process)
                    │ Unix socket (framed protocol)
                    ▼
┌────────────────────────────────────────────────────┐
│ pterm daemon                                       │
│ ├─ owns session PTY                                │
│ ├─ tracks terminal state via vt100 parser          │
│ ├─ multiplexes multiple clients                    │
│ └─ exits when session socket is removed            │
└───────────────────┬────────────────────────────────┘
                    │
                    ▼
              Child process (shell/TUI)
```

## Why Attach Bridge + `term=true`

Earlier Lua-based rendering (`nvim_open_term` + `nvim_chan_send`) made the Lua scheduler part of the hot path.

Current approach:

- renders through Neovim's native terminal data path
- avoids Lua-side terminal byte parsing
- keeps native rendering separate from session persistence and PTY control

## Components

### Lua plugin (`lua/pterm/init.lua`)

Responsibilities:

- binary discovery from the plugin worktree, Nix build output, or `PATH`
- session create/list/kill orchestration
- attach lifecycle in Neovim
- select the focused window's text-area size as shared PTY size authority
- reset Neovim's local scrollback before acknowledging managed history replay

Key behavior:

- `:Pterm new <name> [cmd...]`:
1. if the session is already connected in the current Neovim instance, jump to its buffer
2. otherwise run `pterm open <name> --no-resize` in a terminal buffer
3. `pterm open` attaches if the session exists, or creates it and then attaches in one process
4. this single-process `open` path avoids the timing gap between daemon creation and bridge connection that previously caused wrong-size initial snapshots

- attach uses a fresh buffer because `jobstart(..., { term = true })` requires an unmodified current buffer.
- closing/deleting the pterm buffer detaches only; it does not kill the daemon session.
- `flush_resize()` invokes the one-shot `pterm resize` controller. Focus and layout
  callbacks recheck the active buffer before dispatch; passive mirrors do not
  choose the shared size. `FocusLost` stops pending control jobs and
  `FocusGained` reclaims authority for the current pterm window.
- Native terminal sizing can use the largest window displaying a buffer. The
  managed bridge reports that physical renderer size separately, so it cannot
  overwrite the focused window's authoritative size.

### Daemon (`src/main.rs`, `src/server.rs`, `src/session.rs`)

Responsibilities:

- create and own PTY/child process
- keep process alive independently from Neovim
- track terminal state via vt100 parser and provide state snapshots on attach
- serve multiple attach clients over Unix socket

Notable behavior:

- session socket path: `<socket_root>/<session>/socket`
- if socket file is removed externally, daemon treats session as deleted and exits
- a session has one authoritative PTY size. `SET_SIZE` from the managed
  controller and `RESIZE` from standalone attachments change that size; the
  latest processed request wins. All render clients receive replacement state,
  using their own physical dimensions for managed views.
- `VIEW_SIZE` changes only a managed client's physical renderer size. It does
  not resize the shared PTY. No initial managed view is sent until this size is
  known (`Server::send_view_to_client`).
- the vendored vt100 parser reflows the primary grid and its retained history
  on size changes; the alternate grid uses ordinary resize. An application
  must redraw its alternate-screen layout. Actual application newlines are
  preserved, not guessed into paragraphs.
- output delivery uses per-client send queues and writable polling to avoid disconnecting on backpressure (`WouldBlock`)
- **managed delivery**: `VIEW` contains a cropped/padded rendering of canonical
  state, with history generations deciding whether to append or replace local
  history. These clients do not receive the raw PTY `OUTPUT` broadcast.
  One-shot events such as BEL and clipboard writes use `VIEW_EVENT` and are not
  replayed on refresh.
- **raw/snapshot delivery**: standalone clients receive `OUTPUT` after their
  initial snapshot, with `HISTORY` once before `STATE_SYNC` if requested. A
  resize replaces queued state; redraw does not append the same history again.
  Clients just given a snapshot are excluded from that flush's raw broadcast.
- **drain-and-flush**: PTY output uses non-blocking drain (reads until `WouldBlock`) followed by immediate flush — no timer-based micro-batching, minimizing latency while naturally coalescing bytes available at each poll cycle
- EXIT message is queued into `send_buf` (not written directly) to preserve OUTPUT→EXIT ordering under backpressure, and is sent exactly once via an `exit_sent` guard

### Bridge (`src/bridge.rs`)

Responsibilities:

- stdin -> daemon `INPUT`
- daemon views/events or raw output/snapshots -> stdout
- report native size changes (`SIGWINCH` -> `VIEW_SIZE` for managed attachments,
  `RESIZE` for standalone attachments)
- hold managed replacement bytes until the Lua scrollback-reset acknowledgement

Implementation notes:

- nonblocking `libc::poll` loop (the daemon uses `mio`)
- raw mode guard for terminal settings restoration
- framed protocol parsing with buffered partial-frame handling
- `EINTR` on `poll` is retried
- bridge is not a pure byte-for-byte relay: before replaying `STATE_SYNC` it injects terminal cleanup for keyboard protocol state, and on detach it emits cleanup sequences so the next shell prompt does not inherit TUI modes
- output queues preserve frame/cleanup order across partial writes and pause
  daemon reads at the output watermark while input and resize remain responsive

### Managed history replacement

The daemon chooses replacement history from canonical state and flags the `VIEW`.
It defers primary-history replacement while alternate screen is active until
the primary screen returns. The bridge and Lua plugin then coordinate local
history replacement:

1. `HistoryReset` in `src/bridge.rs` holds replacement bytes and emits the
   `OSC 51; pterm-reset-history` request after preparing a blank sentinel row.
2. The plugin's `TermRequest` callback clears local history through the
   `scrollback` option, refreshes capacity, restores the saved option, and sends
   `OSC 51; pterm-history-ready` only after those steps.
3. The bridge consumes that ACK without forwarding it to the application,
   sends `CSI 3 J`, and releases the held bytes in order. ACK waiting is bounded
   by a 30-second timeout; repeated replacements keep the latest replacement.

This also covers Neovim versions without a `CSI 3 J` scrollback-clear callback.
The Lua callback owns Neovim option handling, the bridge owns ACK ordering, and
the daemon owns the source history. Replaying history without replacing the
local copy would duplicate it.

## Wire Protocol

All messages are framed:

```text
[type: u8][length: u32 little-endian][payload bytes]
```

Protocol version is 2. The complete message definitions, payloads and limits
live in [`proto/src/lib.rs`](../proto/src/lib.rs). The main control/render paths are:

| Message | Direction | Responsibility |
|---|---|---|
| `HELLO` / `HELLO_ACK` | Both | Version handshake; `REQUEST_HISTORY` and `CANONICAL_VIEW` select replay/render behavior |
| `INPUT` / `INPUT_ORIGIN` | Client -> daemon | Application input and ordered managed mouse-origin metadata |
| `SET_SIZE` / `RESIZE_ACK` | Both | One-shot authoritative PTY resize; ACK follows processing that request |
| `RESIZE` | Client -> daemon | Standalone bridge's authoritative PTY resize |
| `VIEW_SIZE` | Client -> daemon | Managed physical renderer size, without PTY authority |
| `VIEW` / `VIEW_EVENT` | Daemon -> client | Managed ANSI state/history with reset flag, or a one-shot event |
| `HISTORY` / `STATE_SYNC` / `OUTPUT` | Daemon -> client | Raw/snapshot path: initial history, reconstructed state, subsequent filtered PTY output |
| `REDRAW` | Client -> daemon | Refresh render clients through their respective paths |
| `EXIT` | Daemon -> client | Child exit code, ordered after queued output |

Managed attachments reject pre-v2 daemons and legacy `STATE_SYNC` rendering;
standalone mode retains version-mismatch notices on its snapshot path. These
checks do not provide in-place daemon migration. Updating the binary/plugin does
not replace a running daemon. Restarting a session ends its child process, so
session migration must be planned explicitly; the plugin does not auto-restart it.

## Regression Coverage

- `src/server.rs::canonical_view_size_is_independent_and_managed_output_is_not_raw`
  checks size authority, canonical-only delivery and `SET_SIZE` acknowledgement.
- `src/bridge.rs` unit tests cover split ACKs, replacement coalescing and input
  preservation; `tests/neovim_regressions.lua` checks option restoration before ACK.
- `tests/neovim_resize_regressions.lua` exercises real PTYs, narrow/resized mirrors,
  Unicode history and alternate/primary transitions. These live fixtures are
  opt-in; `flake.nix` runs the mocked Neovim suite in its normal checks.
- `tests/cli.rs` covers framed delivery, EOF/exit ordering and output backpressure.

### Managed connection negotiation

A newly accepted socket uses the legacy output path until its `HELLO` is
processed. A concurrent resize/redraw request from another client, or PTY
output, can send `STATE_SYNC`/`OUTPUT` before that point. Their arrival alone
does not prove that the daemon lacks canonical rendering support.

A managed (`--no-resize`) bridge discards raw `STATE_SYNC`, `OUTPUT` and
`HISTORY` frames until `HELLO_ACK` arrives. The subsequent canonical `VIEW`
reconstructs the current display and retained history. Waiting for the ACK is
bounded to three seconds; if an old daemon never acknowledges, the bridge
returns a restart/upgrade error. A protocol version below 2 in the ACK, or `STATE_SYNC`
after negotiation, still fails immediately. Standalone bridges and passive
legacy socket clients keep their existing output behavior.

Regression coverage: `managed_bridge_ignores_raw_frames_sent_before_hello_ack`
and `managed_bridge_bounds_hello_ack_wait_for_an_old_daemon` in `tests/cli.rs`.

## Socket and Session Layout

Socket root directory resolution order:

1. `PTERM_SOCKET_DIR`
2. `XDG_RUNTIME_DIR/pterm`
3. `/tmp/pterm-$UID`

Hierarchical sessions are represented by directories:

```text
<root>/project/socket
<root>/project/build/socket
```

## Lifecycle and Deletion Rules

- Detach (buffer close / job stop) does not delete session.
- Session deletion is explicit via `pterm kill` / `:Pterm kill`, or by removing the session socket file externally.
- plugin code should not remove socket files automatically.
- `pterm kill <parent>` removes the parent session directory recursively, so hierarchical children under that prefix are deleted too.

## Known Limitations / TODO

- persistence across reboot (save/restore scrollback)
- in-place migration of older daemon sessions
- optional health/reconnect diagnostics
- signal forwarding policy beyond raw input (if needed)

# ADR-0001: Rendering Corruption Mitigations

**Status**: Implemented (original mitigations)

**Original branch**: `fix/rendering-corruption`

---

## Context

This ADR records the original raw/snapshot-path corruption mitigations. That
path uses a two-layer snapshot model:

1. `vt100::Screen::state_formatted()` — reconstructs terminal state from what the `vt100` crate tracks internally
2. `SessionCallbacks::passthrough_sequences` — bounded queue (256 seqs / 16 KiB) capturing sequences `vt100` does not handle

On attach/reconnect, `build_snapshot()` combines these layers. The original risks
were stale-size snapshots, conflicting clients and state lost from bounded raw
sequence retention. Managed Neovim attachments now use canonical views instead
of this raw replay path; see [current architecture](../DESIGN.md).

---

## Original Decisions and Current Implementation

### ✅ DONE — RESIZE stale-buffer ordering (PR #43)

**Problem**: After `RESIZE`, stale `OUTPUT` frames or an old-size snapshot could remain queued in the client's send buffer ahead of the new-size snapshot, causing the bridge to replay bytes sized for the wrong terminal dimensions.

**Implementation**: `Server::send_snapshot_to_client` replaces unsent queued
state before enqueuing the new snapshot; partially sent frames keep their framing.
Managed clients dispatch to `send_view_to_client`, which also replaces stale
queued views. Resize updates all render clients, not only those awaiting their
first snapshot. Diagnostic query responses are excluded from replacement.

**Coverage**: `src/server.rs` tests
`resize_during_output_backlog_keeps_frame_stream_valid` and
`resize_does_not_drop_queued_diagnostic_response`.

---

### ✅ DONE — Corruption detection logging (PR #42 / refactor branch)

**Problem**: Rendering corruption was silent and hard to correlate with server events.

**Decision**: Added `warn!`/`debug!` log points for passthrough overflow, DA leakage, snapshot race window, send-buf backpressure, and unknown CSI/ESC sequences.

---

### ✅ DONE — Multi-client resize contention (TODO-1)

**Severity**: High  
**Status**: Implemented; managed size authority is now separate from view size.

**Original problem**: The session has a single shared PTY, but only the resizing
client used to receive the replacement snapshot.

**Implementation**: `RESIZE` and one-shot `SET_SIZE` change shared PTY dimensions
and send replacement state to every render client. The focused managed window
chooses `SET_SIZE` through the Lua plugin's `pterm resize` command; its bridge's
`VIEW_SIZE` reports only physical renderer size. Passive managed views crop/pad
canonical state without acquiring PTY size authority. Standalone CLI `RESIZE`
requests still change the shared size; the latest processed authority request wins.

**Coverage**: `src/server.rs::canonical_view_size_is_independent_and_managed_output_is_not_raw`
and `tests/neovim_resize_regressions.lua`.

---

### ✅ DONE — DA1/DA2 queries unanswered while client is attached (TODO-2)

**Severity**: Medium  
**Status**: Implemented, with PTY ECHO checks.

**Original problem**: DA1/DA2 replies depended on `clients.is_empty()`, so attached
applications could lose replies to terminal-identity probes.

**Implementation**: `Server::drain_pty_output` queues DA1/DA2 replies regardless
of client attachment state when PTY ECHO is disabled. Replies are deliberately
dropped when ECHO is enabled or its state cannot be read, to avoid echoed query
responses leaking into terminal output. Session callbacks count queries and the
output filter removes reply-generating queries from the raw client stream.

**Evidence**: `src/server.rs::drain_pty_output`,
`src/session.rs::SessionCallbacks::unhandled_csi` and the
`terminal_output_filter_strips_reply_generating_queries` regression.

---

### ✅ DONE — Stateful sequences relying solely on passthrough replay (TODO-3)

**Severity**: Medium  
**Status**: The listed high-value modes have structured state; support for other
terminal extensions remains incremental.

**Original problem**: Long-lived modes depended on raw sequences surviving the
bounded passthrough queue, including both ends of state-changing pairs.

**Implementation**: `SessionCallbacks` explicitly stores cursor shape, separate
main/alternate Kitty keyboard flags and stacks, focus tracking, synchronized
output and the currently open OSC 8 URI. `build_snapshot` reconstructs that
state rather than depending on queue retention. Managed views use
`append_canonical_tracked_state`; one-shot events are delivered separately and
are not replayed as persistent state.

**Coverage**: `src/session.rs` tests
`snapshot_preserves_decscusr_cursor_shape`,
`snapshot_preserves_focus_tracking_and_synchronized_output`,
`snapshot_replays_kitty_keyboard_stack_and_current_flags`,
`snapshot_preserves_distinct_main_and_alternate_kitty_keyboard_states` and
`snapshot_replays_open_hyperlink_only`.

---

## Current Boundaries

- The original TODO numbers above are historical identifiers, not open work.
- The bounded passthrough queue still cannot guarantee reconstruction of every
  unmodelled terminal extension. Working-directory display uses the daemon's
  process-CWD lookup (`src/paths.rs`); this ADR does not promise OSC 7 restoration.
- Managed history replacement belongs to the daemon/bridge/Lua handshake
  described in [Design](../DESIGN.md#managed-history-replacement). Neovim's local
  scrollback is a rendering copy, not the canonical history store.
- Managed attachments require protocol 2. Upgrading the plugin/binary does not
  migrate an existing daemon; neither automatic restart nor preservation of its
  child process across restart is provided.

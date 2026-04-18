# ADR-0001: Rendering Corruption Mitigations

**Status**: In Progress  
**Branch**: `fix/rendering-corruption`

---

## Context

pterm uses a two-layer snapshot model:

1. `vt100::Screen::state_formatted()` — reconstructs terminal state from what the `vt100` crate tracks internally
2. `SessionCallbacks::passthrough_sequences` — bounded queue (256 seqs / 16 KiB) capturing sequences `vt100` does not handle

On client attach/reconnect, `build_snapshot()` combines both layers and replays the byte stream into the Neovim terminal buffer. This design exposes several correctness risks when terminal state cannot be fully reconstructed from these two sources alone.

---

## Decisions and TODOs

### ✅ DONE — RESIZE stale-buffer ordering (PR #43)

**Problem**: After `RESIZE`, stale `OUTPUT` frames or an old-size snapshot could remain queued in the client's send buffer ahead of the new-size snapshot, causing the bridge to replay bytes sized for the wrong terminal dimensions.

**Decision**: `send_snapshot_to_client` now clears the client's `send_buf` before enqueuing the fresh snapshot. `RESIZE` always issues a replacement snapshot regardless of `pending_snapshot` state.

---

### ✅ DONE — Corruption detection logging (PR #42 / refactor branch)

**Problem**: Rendering corruption was silent and hard to correlate with server events.

**Decision**: Added `warn!`/`debug!` log points for passthrough overflow, DA leakage, snapshot race window, send-buf backpressure, and unknown CSI/ESC sequences.

---

### 🟡 TODO-1 — Multi-client resize contention

**Severity**: High  
**Status**: Decided — not yet implemented

**Problem**: The session has a single shared PTY. Any client's `RESIZE` changes the PTY dimensions for all clients. Only the resizing client receives a fresh replacement snapshot; other attached clients silently end up with a mismatched terminal size.

**Decision**: Last-write-wins. The most recent `RESIZE` from any client becomes the authoritative PTY size. All other attached clients receive a replacement snapshot at the new dimensions immediately after the resize.

**Files to change**: `src/server.rs` (RESIZE handler — broadcast replacement snapshot to all clients, not only the resizing one)

---

### 🟡 TODO-2 — DA1/DA2 queries unanswered while client is attached

**Severity**: Medium  
**Status**: Not yet fixed

**Problem**: Daemon auto-answers DA1/DA2 only when `clients.is_empty()`. When a client is attached, the query is silently dropped. Applications that probe terminal identity after attach or after a reset receive no reply, which can cause hangs or incorrect behavior.

**Decision**: Always reply to DA1/DA2 from the daemon, regardless of client attachment state. The daemon intercepts these queries anyway; there is no reason to suppress the reply when clients are present.

**Files to change**: `src/server.rs` (`handle_pty_output`, DA reply block)

---

### 🟡 TODO-3 — Stateful sequences relying solely on passthrough replay

**Severity**: Medium  
**Status**: Ongoing / gradual improvement

**Problem**: Several long-lived terminal modes survive reconnect only because their raw escape sequences are still in the bounded passthrough queue. If they are evicted (queue full) or if the open/close pair is split across eviction boundaries, the replayed state is inconsistent.

Modes currently at risk:
- Cursor shape (`DECSCUSR`) — already in passthrough allowlist, but raw replay only
- Kitty keyboard protocol (`CSI > Ps u` etc.)
- Focus tracking (`?1004`)
- Synchronized output (`?2026`)
- OSC 8 hyperlinks — open/close pairs can be split by eviction
- Hyperlink state, OSC 7 (working directory)

**Decision**: Promote high-value modes to explicit state fields in `SessionCallbacks`, rather than relying on raw byte retention. Priority order:

1. Cursor shape — track as `Option<u8>` (DECSCUSR `Ps` value)
2. Kitty keyboard flags — track as bitmask
3. Focus tracking / synchronized output — track as `bool`
4. OSC 8 hyperlinks — track as `Option<String>` (current open URI)

**Files to change**: `src/session.rs` (`SessionCallbacks`, `build_snapshot`)

---

## Priority Order

| # | TODO | Severity | Effort | Suggested PR |
|---|------|----------|--------|--------------|
| 1 | TODO-2: DA query reply | Medium | Small | Add to `fix/rendering-corruption` |
| 2 | TODO-1: Multi-client resize broadcast | High | Medium | Separate PR |
| 3 | TODO-3: Structured state for top modes | Medium | Medium–Large | Separate PR per mode |

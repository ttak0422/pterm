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

### 🟢 TODO-4 — Passthrough overflow in high-OSC workloads

**Severity**: Low (normal Neovim editing), Medium (OSC-heavy apps)  
**Status**: Logged but not structurally fixed

**Problem**: The 256 seq / 16 KiB FIFO evicts oldest entries first. Normal Neovim editing is unlikely to overflow (cursor-shape changes, title updates, and focus events are sparse). However, terminal apps emitting frequent OSC 8 hyperlinks, OSC 52 clipboard writes, or desktop notifications (OSC 99) can realistically overflow.

**Decision**: The logging added in PR #42 is sufficient for now. When TODO-3 promotes common modes to structured state, overflow risk shrinks naturally. Revisit limits after structured state work lands.

---

### 🟢 TODO-5 — Reconnect regression test coverage

**Severity**: Low  
**Status**: Not yet added

**Problem**: `vt100`'s `state_formatted()` correctness is assumed. If `vt100` mishandles a sequence internally (emits wrong output), neither the passthrough queue nor the logging catches it.

**Decision**: Add integration-level reconnect tests for real Neovim workflows:
- Alternate-screen enter/exit (Neovim startup/shutdown)
- Cursor-shape transitions (normal ↔ insert mode)
- Hyperlink open/close round-trip
- Title push/pop stack
- Resize-after-attach

**Files to change**: `src/session.rs` (test module)

---

## Priority Order

| # | TODO | Severity | Effort | Suggested PR |
|---|------|----------|--------|--------------|
| 1 | TODO-2: DA query reply | Medium | Small | Add to `fix/rendering-corruption` |
| 2 | TODO-1: Multi-client resize | High | Medium | Separate PR after design confirmed |
| 3 | TODO-3: Structured state for top modes | Medium | Medium–Large | Separate PR per mode |
| 4 | TODO-5: Reconnect regression tests | Low | Medium | Can accompany TODO-3 PRs |
| 5 | TODO-4: Passthrough overflow | Low | Deferred | Revisit after TODO-3 |

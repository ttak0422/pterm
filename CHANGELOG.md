# Changelog

All notable changes to this project will be documented in this file.

## [1.0.1] - 2026-04-29

### Miscellaneous Tasks

- *(actions)* Update app token action for node24
## [1.0.0] - 2026-04-18

### Features

- Auto-redraw on BufEnter/TermEnter to fix rendering corruption
Register BufEnter and TermEnter autocmds on each pterm buffer so that
  switching window focus or re-entering terminal mode automatically
  triggers a redraw, recovering from rendering corruption caused by
  mid-flight interruptions.

  Debounce (50 ms default) avoids redundant redraws when both events fire
  together. The feature is on by default and can be disabled via
  `setup({ auto_redraw = false })`.
- *(logging)* Add rendering corruption detection log points
Adds targeted warn!/debug! calls to help correlate rendering glitches
  with observable server-side events, without changing any behavior.

  Detectable signals:
  - warn: passthrough sequence dropped due to buffer overflow (>256 seqs /
    >16 KiB); next snapshot will be missing those sequences
  - warn: DA1/DA2 query pile-up while clients are attached; replies may be
    duplicated or misdirected across multiple clients
  - warn: client send buffer backlog exceeds 64 KiB; burst flush after
    stall can cause visual tearing
  - debug: snapshot sent while PTY bytes are still buffered (race window)
  - debug: unhandled CSI dropped from snapshot replay path (hex dump)
  - debug: unhandled ESC preserved as passthrough (hex dump)

### Bug Fixes

- Guard stale timer callback and cache binary path
- Add identity check in schedule_redraw callback so that a stale timer
    fired after being replaced cannot nil-out the newer timer's entry in
    redraw_timers, fixing a debounce race condition with vim.schedule_wrap
  - Cache the result of find_binary() so repeated BufEnter/TermEnter
    events no longer pay the filesystem lookup cost each time
- Replace stale-size snapshot on RESIZE to prevent dimension mismatch
If a client sent RESIZE after the server had already queued a snapshot
  (or queued raw OUTPUT frames), the bridge could replay bytes sized for
  the old terminal dimensions into the newly-sized Neovim window, causing
  rendering corruption.
- DA always reply, resize broadcast, and structured terminal state
Fix 1 — DA1/DA2 always reply (src/server.rs):
  Daemon now answers pending DA1/DA2 queries regardless of client
  attachment state. Previously queries were silently dropped while a
  client was connected, causing applications that probe terminal identity
  after attach or reset to hang.

  Fix 2 — Multi-client resize broadcast (src/server.rs):
  After RESIZE, replacement snapshots are now sent to ALL attached
  clients (not only the resizing one), with replace_send_buf=true so
  stale-size output frames are cleared for every client. Implements
  last-write-wins: the most recent RESIZE is authoritative for all.

  Fix 3 — Promote stateful sequences to structured state (src/session.rs):
  Four long-lived terminal modes that previously survived reconnect only
  via the bounded passthrough queue are now stored as explicit fields on
  SessionCallbacks and reconstructed deterministically in build_snapshot():
  - cursor_shape: Option<u8>  (DECSCUSR Ps value)
  - kitty_keyboard_flags: Option<u32>  (CSI >/= Ps u; cleared by CSI < u)
  - focus_tracking: bool  (DEC private mode 1004)
  - synchronized_output: bool  (DEC private mode 2026)
  - hyperlink_uri: Option<String>  (OSC 8 open URI)

  Modes 1004 and 2026 removed from PASSTHROUGH_DEC_PRIVATE_MODES.
  Tests added for kitty flag replay and open hyperlink replay.
- Trigger auto redraw immediately
- Move find_binary before trigger_redraw to fix nil call error
trigger_redraw referenced find_binary before it was declared as a local,
  causing Lua to resolve it as a global (nil). Exposed by PR #47 which added
  an immediate trigger_redraw() call in schedule_redraw().

### Documentation

- Document auto_redraw options in README and CONFIGURATION.md
- Add ADR-0001 for rendering corruption mitigations
Records findings from Codex + Zellij source analysis:
  - DONE: RESIZE stale-buffer fix (PR #43)
  - DONE: corruption detection logging (PR #42)
  - TODO-1 (high): multi-client resize contention — architecture decision needed
  - TODO-2 (medium): DA1/DA2 always reply regardless of client attachment
  - TODO-3 (medium): promote stateful sequences to explicit SessionCallbacks fields
  - TODO-4 (low): passthrough overflow in high-OSC workloads — deferred
  - TODO-5 (low): reconnect regression test coverage
- *(adr)* Decide last-write-wins for multi-client resize
On RESIZE, broadcast a replacement snapshot to all attached clients
  (not only the resizing one) so every client stays aligned with the
  new PTY dimensions.
- *(adr)* Drop TODO-4 and TODO-5 from ADR-0001
- *(adr)* Mark all TODOs as completed in ADR-0001
- Refresh design notes for open flow

### Miscellaneous Tasks

- Increase default auto_redraw_delay_ms from 50 to 1000
Rendering corruption does not occur within milliseconds of focus
  changes, so a 1 second debounce is sufficient and avoids unnecessary
  redraws during rapid window switching.
- Remove harper-dictionary and add PR template
- drop .harper-dictionary.txt (noise, not worth maintaining)
  - add .github/pull_request_template.md: English-only, AI-filled checklist
    (CHANGELOG is automated via git-cliff so excluded)
## [0.2.6] - 2026-04-11

### Bug Fixes

- Preserve unhandled OSC/CSI/escape sequences in snapshots
vt100's state_formatted() drops sequences it does not handle (e.g. OSC 8
  hyperlinks). Implement unhandled_osc, unhandled_escape, and unhandled_csi
  on SessionCallbacks to capture and re-emit those sequences as a prefix in
  build_snapshot(), so attach/reconnect sees the same output as live display.

  Passthrough storage is bounded at 256 sequences / 16 KiB to prevent
  unbounded growth. Adds two regression tests: one confirming OSC 8 survives
  the snapshot round-trip, one confirming SGR (handled by vt100) is NOT
  duplicated into the passthrough buffer.
- Preserve cursor shape (DECSCUSR) across snapshots and reset on detach
Vim emits DECSCUSR sequences (ESC[Ps SP q) when switching between
  normal/insert mode to change cursor shape (e.g. block vs bar). These
  sequences were silently dropped, causing the cursor shape to appear
  frozen or incorrect after a snapshot replay.

  Three related fixes:

  1. `unhandled_csi`: Preserve DECSCUSR sequences in the passthrough
     buffer instead of discarding them. vt100 does not handle DECSCUSR
     internally and state_formatted() never emits them, so they must be
     captured here.

  2. `format_unhandled_csi`: Fix byte ordering for true intermediate bytes
     (0x20-0x2F, e.g. SP in DECSCUSR). These must be emitted after
     numerical parameters, not before. Private prefix bytes (0x3C-0x3F,
     e.g. ?) continue to be emitted before parameters.

  3. `build_snapshot`: Move passthrough sequences to the end of the
     snapshot, after state_formatted() output. state_formatted() emits
     cursor-positioning sequences at the end; passthrough sequences
     (including DECSCUSR) must follow so they are not overwritten.

  4. `DETACH_CLEANUP_SEQUENCES`: Add ESC[0 q to reset cursor shape on
     detach so the enclosing terminal is not left with Vim's cursor style.
- Handle cursor blink, DECLRMM, and window title stack in snapshots
Three additional Vim-related control sequences were silently dropped,
  causing state loss across snapshot replay:

  1. ESC[?12h/l (AT&T 610 cursor blink): vt100 does not handle mode 12
     in decset/decrst, so it reaches unhandled_csi.  Preserve it in the
     passthrough buffer so replay restores the blink state.

  2. ESC[?69h/l (DECLRMM left-right margin mode): same path as above.
     Also add ESC[?69l to DETACH_CLEANUP_SEQUENCES so the mode is reset
     when the user detaches.

  3. ESC[22;0t / ESC[23;0t (window title save/restore): Vim saves the
     original terminal title on entry and restores it on exit via these
     sequences.  Track a title stack in SessionCallbacks; build_snapshot
     now replays the stack so a subsequent ESC[23;0t after attach pops
     the correct title.
- Preserve Neovim control sequences and refactor passthrough logic
Add passthrough handling for sequences that vt100 does not track but
  Neovim uses in normal operation, so they survive snapshot replay on
  re-attach:

  - ESC[?1004h/l: focus tracking (Neovim emits on startup)
  - ESC[?2026h/l: synchronized output
  - CSI > Ps u / CSI = Ps u / CSI < u: Kitty keyboard protocol
  - SGR 4:N (undercurl) and SGR 58:2:r:g:b (underline color)
  - OSC 52: clipboard copy/paste via copy_to_clipboard callback
  - CSI Nt (t sequences other than 22/23) are now passed through
    instead of being silently dropped

  Also add ESC[?1004l and ESC[?2026l to DETACH_CLEANUP_SEQUENCES so
  focus-tracking and synchronized-output modes are reset on detach.
## [0.2.5] - 2026-04-01

### Features

- Auto-push tag when release PR is merged
リリースPR（release/v*ブランチ）がmainにマージされると
  GitHub Appトークンで自動的にタグをpushし、releaseワークフローをトリガーする

### Refactor

- Merge tag-release into release workflow
tag-releaseとreleaseを統合し、リリースPRマージ時に
  タグ作成とGitHub Release作成を1つのワークフローで完結させる
## [0.2.4] - 2026-04-01

### Bug Fixes

- Quote colon-containing strings in release-prep workflow
YAML requires quoting values that contain colons. The title and
  commit-message fields were causing a parse error on line 38.
- Use GitHub App token for release prep PR creation
- Add actions/create-github-app-token@v2 to generate token
  - Update peter-evans/create-pull-request v7 -> v8 (Node.js 24対応)
  - PRをGitHub Appとして作成することでCIワークフローがトリガーされる
- Allow ttak0422-bot in claude code review workflow
## [0.2.3] - 2026-03-28

### Features

- Add nix app for testing pterm in isolated neovim
Add `nix run .#test-nvim` app that launches Neovim with pterm
  pre-loaded for quick manual verification without manual setup.

### Bug Fixes

- Update vt100 0.15 → 0.16 to restore dim (SGR 2) attribute in snapshots (#16)
vt100 0.15 did not track the dim/faint attribute (SGR 2) at all — no
  TEXT_MODE_DIM bit and no handler in sgr(). As a result, state_formatted()
  never re-emitted ESC[2m, so text rendered as dim in the live session would
  appear as normal-weight text after a detach/re-attach cycle.

  vt100 0.16 adds TEXT_MODE_DIM and handles SGR 2 → set_dim(), and
  state_formatted() correctly re-emits Intensity::Dim as ESC[2m.

  Confirmed via byte-level comparison of live PTY output vs snapshot:
    ORIGINAL:  ESC[2mNo ESC[0m  (faint)
    0.15 snap: ESC[m  No        (dim lost)
    0.16 snap: ESC[m  ESC[2mNo  (dim preserved)

  API changes in 0.16:
  - screen.errors() removed → dropped from debug log format string
  - parser.set_size(rows, cols) → parser.screen_mut().set_size(rows, cols)
- Improve session snapshot and terminal lifecycle management (#17)
* fix: improve session snapshot and lifecycle management

  - (H) Send terminal cleanup sequences on detach to reset mouse modes,
    bracketed paste, alternate screen, and cursor visibility in the
    client terminal
  - (B) Increase scrollback capacity from 0 to 10,000 lines so history
    is available after reattach
  - (G) Respond to DA1/DA2 queries with canned responses when no clients
    are connected, preventing apps from hanging on capability detection
  - (A) Prepend ESC[?1049h in snapshots when alternate screen is active
    so reattaching clients properly enter alternate screen mode
  - (C) Track window title via vt100 Callbacks and restore it in
    snapshots via OSC 2 sequence on reattach
## [0.2.2] - 2026-03-11

### Bug Fixes

- Defer BufDelete handler to ignore buflisted changes from scope-nvim
scope-nvim scopes buffers per tab page by toggling buflisted on
  TabLeave/TabEnter. Setting buflisted=false fires BufDelete, which
  pterm's handler interpreted as a true buffer deletion—calling
  M.detach(), killing the bridge (SIGHUP/129), and wiping the buffer.

  Defer the BufDelete callback with vim.schedule and check whether the
  buffer is still valid and loaded. A merely-unlisted buffer survives
  the check and the connection is preserved; a truly deleted buffer
  triggers detach as before.
## [0.2.1] - 2026-03-10

### Bug Fixes

- Prevent duplicate rendering when attaching via :Pterm user command
When a client with pending_snapshot=true received a snapshot triggered
  by PTY output arrival, the same flush cycle also broadcast the raw
  OUTPUT bytes to that client.  Since the snapshot already reflects the
  effect of those bytes (read_pty feeds data to the VT parser before
  flush), Neovim's libvterm processed the same content twice, causing
  garbled/duplicated rendering (e.g. "e" → "ee" → "eechooo").

  This was not observed with toggleterm because `pterm open` connects
  the bridge in the same process immediately after daemon creation,
  before significant PTY output is produced.  The :Pterm user command
  creates the daemon separately (`pterm new` via vim.fn.system), so by
  the time the bridge attaches the shell prompt is already in the VT
  state, making the race between snapshot and OUTPUT much more likely.
- Use pterm open to eliminate snapshot timing gap in :Pterm command
When :Pterm created a session via the two-step flow (vim.fn.system for
  `pterm new` → wait_for_socket → jobstart for `pterm attach`), the shell
  had time to produce PTY output during the gap. On bridge connection,
  server.rs flush_pty_output() triggered snapshot delivery at the old
  80×24 size BEFORE the client's RESIZE was processed, leaving libvterm
  with wrong scroll regions and causing progressive display corruption.

  CLI `pterm open` was unaffected because it handles creation and bridge
  attachment in a single process with minimal delay.

### Miscellaneous Tasks

- Update CHANGELOG.md on release
Add post-release steps to regenerate CHANGELOG.md using git-cliff
  and commit it back to the default branch.
## [0.2.0] - 2026-03-08

### Features

- *(server)* Handle redraw request
- *(cli)* Add redraw command
- *(nvim)* Add PtermRedraw command
- Add Telescope extension for pterm sessions
Add lua/telescope/_extensions/pterm.lua providing a Telescope picker
  (:Telescope pterm sessions) to fuzzy-search and jump to pterm sessions.
  Connected sessions are visually marked as [connected]. Selecting a stale
  session shows an error notification without crashing. Also auto-loads
  the extension from M.setup() when Telescope is available.

### Bug Fixes

- *(server)* Improve error handling and flush after redraw broadcast
Gracefully handle client I/O errors in the event loop instead of
  propagating them, and ensure all clients are flushed after a redraw
  broadcast so the snapshot is delivered immediately.
- *(vim)* Stabilize pterm session completion
- *(server)* Coalesce PTY output into single message per poll cycle
Set the PTY master fd to non-blocking after openpty() and add a drain
  loop in handle_pty_output that reads all available data before sending
  a single OUTPUT message to clients. This prevents fragmented prompt
  rendering (e.g. "~/path ma" → "~/path mai" → "~/path main") caused
  by forwarding each small PTY read as a separate protocol message.

  - pty.rs: set O_NONBLOCK on master fd
  - session.rs: map EAGAIN/EWOULDBLOCK to io::ErrorKind::WouldBlock
  - server.rs: drain loop aggregates all reads into one OUTPUT message
- *(bridge)* Batch OUTPUT writes per poll cycle
Collect all OUTPUT/SCROLLBACK payloads from protocol frames into a
  single buffer and write once to stdout, instead of writing each frame
  individually. This further reduces rendering fragmentation on the
  client side.
- *(server)* Add bounded micro-batching for PTY output coalescing
Introduce tmux-style micro-batching to prevent prompt fragmentation on
  fast machines where poll→read outruns shell writes. Small PTY outputs
  are held up to 1ms (max 3ms burst) before flushing, while large outputs
  (≥4096 bytes) flush immediately.

  Also fixes:
  - EXIT message ordering: queue into send_buf instead of direct write_all
    to preserve OUTPUT→EXIT ordering under backpressure
  - Prevent duplicate EXIT broadcast via exit_sent flag
  - Enforce size bound during drain loop (mid-loop flush)
  - Gate deadline refresh on actual new reads
- *(server)* Avoid OUTPUT before initial snapshot
- *(server)* Replace timer-based batching with drain-and-flush
Remove BATCH_DELAY_MS/BATCH_MAX_MS/BATCH_FLUSH_SIZE micro-batching and
  SNAPSHOT_DEFER_MS (500ms) timer. These caused noticeable UX latency.

  New approach:
  - Drain PTY non-blocking until WouldBlock, then flush immediately
  - Snapshot sent on RESIZE or on first PTY OUTPUT (no timer deferral)
  - Bridge-side per-poll-cycle batching retained (zero-delay)

### Refactor

- Remove auto load_extension from setup
Follow the standard Telescope extension convention where users
  explicitly call require("telescope").load_extension("pterm") instead
  of auto-loading during setup. This aligns with how all major Telescope
  extensions (fzf-native, file-browser, frecency, undo, ui-select) work
  and better supports lazy-loading scenarios.

### Documentation

- Rewrite README for Nix users and move detailed config to docs/
- Replace lazy.nvim install section with Nix flake-based instructions
  - Remove verbose sections (Build, How It Works, Session Lifecycle, etc.)
  - Add default configuration example under setup()
  - Move environment variables and socket location to docs/configuration.md
- Update DESIGN.md and proto comments to reflect vt100 state tracking
- Replace "raw byte scrollback ring" with vt100 parser-based terminal
    state tracking in DESIGN.md architecture diagram and daemon section
  - Update SCROLLBACK message description to reflect state_formatted snapshots
  - Remove incorrect proto comment about omitting length field for empty payloads
  - Add trailing newlines to DESIGN.md and proto/src/lib.rs
- Update README to reflect current implementation
Document CLI options (--cols, --rows, -- <command>), hierarchical
  session names, list prefix filter, default session name, exported
  Lua API functions, and clarify cols/rows as fallback values.
- Update DESIGN.md with output coalescing and batching details
- Add Telescope extension section to README

### Styling

- Format bridge and main

### Miscellaneous Tasks

- Add release workflow for tag-triggered changelog generation
- *(docs)* Rename configuration.md to CONFIGURATION.md
## [0.1.0] - 2026-02-26

### Features

- Initial commit
- *(core)* Add open command to create missing session before attach
- *(core)* Replace raw scrollback with vt100 terminal state tracking
Use the vt100 crate to maintain full terminal emulation state instead
  of a raw byte ring buffer. On client attach, generate a precise state
  snapshot via state_formatted() that correctly reproduces cursor
  position, visibility, SGR attributes, scroll region, and input modes.

  This supersedes the previous scrollback sanitization and post-replay
  reset heuristics (Plan B), fixing cursor disappearance and color
  corruption when re-attaching to sessions.

### Bug Fixes

- *(core)* Write all input bytes to pty to avoid garbled redraw
- *(core)* Sanitize scrollback replay to drop terminal queries
- *(core)* Reset terminal state after scrollback replay
Scrollback may contain stale SGR attributes or cursor-hide sequences
  left by applications (vim, fzf, etc.). Without a reset, re-attaching
  clients could see invisible cursors or garbled colors (e.g. zsh
  autosuggestions blending into the background). Append SGR 0 and
  DECCM show-cursor after the scrollback payload to ensure a clean
  terminal state for the live session.

### Documentation

- *(core)* Use actual repository path in lazy.nvim example
- *(core)* Remove cachix section from readme

### Miscellaneous Tasks

- *(core)* Add nix build workflow with major-pinned actions
- *(core)* Set up cachix action for read-only and push modes
- *(core)* Update flake configuration
- Add git-cliff config and generate v0.1.0 changelog
[1.0.1]: https://github.com/ttak0422/pterm/compare/v1.0.0..v1.0.1
[1.0.0]: https://github.com/ttak0422/pterm/compare/v0.2.6..v1.0.0
[0.2.6]: https://github.com/ttak0422/pterm/compare/v0.2.5..v0.2.6
[0.2.5]: https://github.com/ttak0422/pterm/compare/v0.2.4..v0.2.5
[0.2.4]: https://github.com/ttak0422/pterm/compare/v0.2.3..v0.2.4
[0.2.3]: https://github.com/ttak0422/pterm/compare/v0.2.2..v0.2.3
[0.2.2]: https://github.com/ttak0422/pterm/compare/v0.2.1..v0.2.2
[0.2.1]: https://github.com/ttak0422/pterm/compare/v0.2.0..v0.2.1
[0.2.0]: https://github.com/ttak0422/pterm/compare/v0.1.0..v0.2.0


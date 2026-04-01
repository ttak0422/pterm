# Changelog

All notable changes to this project will be documented in this file.

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
[0.2.5]: https://github.com/ttak0422/pterm/compare/v0.2.4..v0.2.5
[0.2.4]: https://github.com/ttak0422/pterm/compare/v0.2.3..v0.2.4
[0.2.3]: https://github.com/ttak0422/pterm/compare/v0.2.2..v0.2.3
[0.2.2]: https://github.com/ttak0422/pterm/compare/v0.2.1..v0.2.2
[0.2.1]: https://github.com/ttak0422/pterm/compare/v0.2.0..v0.2.1
[0.2.0]: https://github.com/ttak0422/pterm/compare/v0.1.0..v0.2.0


# Changelog

All notable changes to this project will be documented in this file.

## [0.1.0] - 2026-02-23

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


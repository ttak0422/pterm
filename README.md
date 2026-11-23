# pterm [![built with nix](https://builtwithnix.org/badge.svg)](https://builtwithnix.org)

Persistent terminal sessions for Neovim.

Processes survive Neovim restarts. Terminal rendering is delegated to Neovim's native terminal (`jobstart(..., { term = true })`) while pterm keeps PTY processes alive.

## CLI

```sh
# Create a new persistent session (forks into background)
pterm new mysession
pterm new mysession -- /bin/zsh        # custom command

# Attach bridge mode (for terminal clients)
pterm attach mysession

# Attach if exists, otherwise create and attach
pterm open mysession
pterm open mysession -- /bin/zsh

# List active sessions (optionally filter by prefix)
pterm list
pterm list myprefix

# Get socket path for a session
pterm socket mysession

# Redraw terminal (resend snapshot to all clients)
pterm redraw mysession

# Print diagnostic state dump as JSON
pterm dump mysession

# Print the visible screen as plain text
pterm snapshot-text mysession

# Print scrollback + visible screen with ANSI colors/attributes preserved
pterm full-text mysession --ansi

# Print the visible screen with ANSI colors/attributes preserved
pterm snapshot-ansi mysession

# Kill a session
pterm kill mysession
```

Session names may contain `/` for hierarchical sessions. Killing a parent session also kills all children.
Names must be relative, with no empty, `.`, `..`, `socket`, or `cwd` components.
Session directories below the socket root must not be symbolic links.

```sh
pterm new parent
pterm new parent/child
pterm kill parent          # kills parent and parent/child
```

## Neovim Usage

```vim
:Pterm new dev      " opens/creates named session
:Pterm new dev zsh  " opens/creates session with custom command
:Pterm list         " list sessions
:Pterm redraw dev   " redraw a session
:Pterm dump dev     " open diagnostic state dump as JSON
:Pterm kill dev     " kill a session
```

`pterm` opens a terminal buffer backed by `jobstart({ "pterm", "attach", <name> }, { term = true })`.

The Lua module also exports functions for programmatic use: `open`, `attach`, `detach`, `list`, `kill`, `redraw`, `dump`.

### Shared-session window size

The most recently focused pterm window chooses the shared session's terminal
width and height. Switching between differently sized splits resizes the shell
or TUI to the newly active window's text area. Background mirrors do not acquire
resize authority merely because their layout changes. Switching back to another
Neovim instance reclaims authority when that editor reports `FocusGained`.

Refocusing a window at the existing session size preserves its normal-mode
cursor and scroll position. Reopening an already connected session also stays
in normal mode instead of forcing terminal input; press `i` or `a` to resume
input at the application's live cursor. New sessions still start in input mode.
When a different window size rebuilds history, normal-mode views are restored
after replay using an exact, unique line-and-neighbor anchor. Existing windows
keep their own positions. Recreated floating-window positions are not retained.
Explicit input or buffer changes cancel stale restoration. If content was clipped,
reflowed, evicted, or the anchor is ambiguous among repeated lines, pterm does not
guess a new position. Both the plugin and bridge must be updated for replay-aware
restoration; old bridges retain their previous behavior.

Other windows show the same canonical terminal screen: larger windows have
padding, and smaller windows clip the view instead of rewrapping the application.
The plugin sends explicit size-control requests and uses a `--no-resize` bridge
so Neovim's automatic largest-window terminal sizing, including delayed bridge
startup, cannot overwrite the active window's dimensions.
Standalone CLI attachments retain their normal SIGWINCH behavior. Managed Neovim
attachments require protocol version 2; restart an older daemon session before
using this plugin version. Extremely large windows are capped at the daemon's
512-column, 256-row and 65,536-cell limits.

When resized history is replayed, a scrollback-reset handshake retains one blank
sentinel line before the refreshed history. This also supports Neovim 0.11 and
older, which cannot clear terminal history with CSI 3 J. Previous history content
is replaced rather than duplicated, and the user's `scrollback` setting is restored.

### TUI redraws

Managed Neovim attachments keep the last complete view while an application
uses synchronized output (DEC mode 2026), rather than displaying each partial
PTY read. A missing end marker is released after a one-second safety timeout
(on the next daemon poll), and child exit releases the final view immediately.
Each generated view is also wrapped in synchronized output so native terminals
that support it, including Neovim 0.12, can paint a whole frame together.
Older Neovim versions still receive complete application updates, but do not
provide atomic rendering of a view delivered across multiple terminal reads.

## Requirements

- Neovim 0.10+
- [Nix](https://nixos.org/) (with flakes enabled)
- Linux (`x86_64-linux`, `aarch64-linux`) or macOS on Apple Silicon (`aarch64-darwin`)

Intel macOS (`x86_64-darwin`) is no longer supported. The Nix flake and macOS CI target Apple Silicon only.

## Install

This plugin is designed to be installed via Nix flakes. Add `pterm` as a flake input and include it in your Neovim plugin list.

### Flake input

```nix
{
  inputs = {
    pterm.url = "github:ttak0422/pterm";
  };
}
```

### Neovim plugin

Add `inputs.pterm.packages.${system}.pterm` to your Neovim plugin list and call `setup()`.

```lua
require("pterm").setup()


-- Default configuration:
require("pterm").setup({
  -- Default shell command
  shell = vim.env.SHELL or "/bin/sh",
  -- Socket directory (nil = let daemon decide)
  socket_dir = nil,
  -- Automatically redraw on BufEnter / TermEnter to recover from rendering
  -- corruption that can occur during mode or window focus switches.
  auto_redraw = true,
  -- Cooldown window in milliseconds for automatic redraws. The first
  -- BufEnter / TermEnter redraw fires immediately; repeated events are
  -- suppressed until the cooldown expires.
  auto_redraw_delay_ms = 1000,
})
```

### Live shell and foreground context

```lua
require("pterm").setup({
  sync_cwd = true,
  track_foreground = true,
})
```

Both options default to false. `sync_cwd` follows the shell's directory with
`:lcd` only in the focused pterm window. `track_foreground` makes the exact
basename of the foreground process-group leader's executable available to
consumers; wrapped or interpreter-based programs may report the wrapper's real
executable. Unknown foregrounds have no value.

`require("pterm").buffer_context(bufnr)` is an in-memory lookup returning
`{ cwd, foreground }` (opt-in fields only) or `nil` if unavailable. It applies
only to pterm buffers, not ordinary `:terminal` buffers. No foreground metadata
is written to disk, interpreted from PTY output, or queried by key mappings.
The optional control socket subscription reuses existing daemon wakeups and
sends only changes; old daemons leave the API unavailable without affecting the
terminal bridge. Live context is sampled, not instantaneous. pterm does not
set filetypes or apply application-specific policies.

## Telescope Extension

pterm provides an optional [Telescope](https://github.com/nvim-telescope/telescope.nvim) extension for fuzzy-finding sessions.

```lua
-- Call after telescope.setup()
require("telescope").load_extension("pterm")
```

```vim
:Telescope pterm sessions
```

Each session is shown as `<name> (<directory>)`, where the directory is the
basename of the session shell's current working directory. The daemon tracks
`cd` inside the session and refreshes it periodically. Connected
sessions are marked with a `✔` prefix.
The preview pane shows the tail of each session's current terminal screen,
trimmed to the preview width, with colors and attributes preserved as
highlighting.
If no existing session matches the current Telescope query, pressing `Enter`
creates or opens a session using the prompt text as the session name.

For full-text search across every session's contents — scrollback history
included — use:

```vim
:Telescope pterm grep
```

Each non-empty line is shown as `<session>: <line>`; pressing `Enter` opens
the session containing the selected line. Lines that wrapped in the terminal
are re-joined before matching, so text spanning a wrap point stays
searchable. Sessions currently showing a full-screen TUI (the alternate
screen, e.g. `less` or `htop`) contribute only their visible screen, not
their shell scrollback. Matching words are highlighted in the result list and
preview. The preview also preserves the original terminal colors and attributes,
including scrollback and re-joined wrapped lines. The selected line is marked
underneath these styles, and matching words are highlighted on top.

Selecting a result navigates to the start of its logical line, including lines
split across terminal rows. The complete search snapshot is checked to distinguish
identical lines. If output has changed, native scrollback was truncated, or the
snapshot has more than one possible location, reopen the picker; pterm does not
guess a different occurrence. Navigation waits up to five seconds for managed
replay and stops if you type, leave/close the window, or replace its buffer.
Existing sessions started by an older daemon remain searchable in plain text;
restart them with the updated daemon to enable colored grep previews.

## Development

Run the Lua linter from the repository root with the pinned Nix toolchain:

```sh
nix develop -c selene lua tests/neovim_regressions.lua tests/neovim_resize_regressions.lua nix/apps/nvim
nix flake check
```

`vim.yml` defines the Neovim APIs used here, including argument checks. Extend it
when adding API calls. Selene also runs in the pre-commit check; `nix flake check`
includes that check and the mocked headless Neovim plugin regressions.
The Rust checks include session/server and CLI integration tests using Unix
sockets. The real Neovim/PTY and tig fixtures in
`tests/neovim_resize_regressions.lua` and `tests/neovim_tig_regressions.lua`
are opt-in development tests and are not run by `nix flake check`.

The grep jump regression tests use native terminal buffers with a mocked picker
and daemon. `nix flake check` also runs `neovim-grep-live` with real Telescope,
Plenary, and disposable daemon sessions. To run that integration test manually,
build `target/release/pterm`, set `PTERM_TELESCOPE_RTP` and `PTERM_PLENARY_RTP` to
plugin checkouts, then run:

```sh
nvim --headless -u NONE -i NONE -l tests/neovim_grep_live_regressions.lua
```

## License

MIT

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

# Kill a session
pterm kill mysession
```

Session names may contain `/` for hierarchical sessions. Killing a parent session also kills all children.

```sh
pterm new parent
pterm new parent/child
pterm kill parent          # kills parent and parent/child
```

## Neovim Usage

```vim
:Pterm dev          " opens/creates named session
:Pterm dev zsh      " opens/creates session with custom command
:PtermList          " list sessions
:PtermRedraw dev    " redraw a session
:PtermKill dev      " kill a session
```

`pterm` opens a terminal buffer backed by `jobstart({ "pterm", "attach", <name> }, { term = true })`.

The Lua module also exports functions for programmatic use: `open`, `attach`, `detach`, `list`, `kill`, `redraw`.

## Requirements

- Neovim 0.10+
- [Nix](https://nixos.org/) (with flakes enabled)
- Linux / macOS

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
  -- Function returning env vars to push into the session on every attach.
  -- Keys are variable names; string values are exported, nil/false values
  -- unset the variable.  Return nil or {} to disable env sync.
  sync_env = function(_session_name)
    return {
      EDITOR               = vim.env.EDITOR,
      VISUAL               = vim.env.VISUAL,
      GIT_EDITOR           = vim.env.GIT_EDITOR,
      NVIM_LISTEN_ADDRESS  = vim.v.servername ~= "" and vim.v.servername or nil,
      NVIM                 = vim.v.progpath   ~= "" and vim.v.progpath   or nil,
    }
  end,
})
```

## Env Sync

When using **zsh**, pterm automatically keeps `EDITOR`, `VISUAL`, `GIT_EDITOR`,
`NVIM_LISTEN_ADDRESS`, and `NVIM` in sync with the Neovim instance that is
currently attached to a session.

On every `:Pterm <session>` call the bridge pushes those values from the active
Neovim into the session.  The next shell prompt sources them, so any command
you run after that (e.g. `git commit`) picks up the right `EDITOR`.

### How it works (zsh)

pterm uses zsh's `ZDOTDIR` mechanism to install a `precmd` hook transparently.
When creating a session pterm:

1. generates a small `zdotdir/` shim inside the session directory,
2. sets `ZDOTDIR` to that directory in the child process environment.

The shim's `.zshenv` and `.zshrc` forward to your original dotfiles and then
register a `precmd` hook that sources the per-session `env.sh` file before
each prompt.  `ZDOTDIR` is unset immediately so child shells you open from
within the terminal behave normally.

> **Note:** pterm sets `ZDOTDIR` only for the shell it spawns.  Your existing
> `ZDOTDIR` (if any) is left untouched for all other processes.

### Other shells

For shells other than zsh the hook must be added manually.  Add to your shell's
rc file:

```bash
# bash
if [[ -n "${PTERM_ENV_FILE:-}" ]]; then
  PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND; }[[ -r \"$PTERM_ENV_FILE\" ]] && source \"$PTERM_ENV_FILE\""
fi
```

### Customising synced variables

Override `sync_env` in your `setup()` call to sync additional variables or
disable the feature entirely:

```lua
require("pterm").setup({
  sync_env = function(_session_name)
    return {
      EDITOR  = vim.env.EDITOR,
      GOPATH  = vim.env.GOPATH,   -- extra variable
    }
  end,
})
```

## Telescope Extension

pterm provides an optional [Telescope](https://github.com/nvim-telescope/telescope.nvim) extension for fuzzy-finding sessions.

```lua
-- Call after telescope.setup()
require("telescope").load_extension("pterm")
```

```vim
:Telescope pterm sessions
```

Connected sessions are shown with a `[connected]` prefix.
If no existing session matches the current Telescope query, pressing `Enter`
creates or opens a session using the prompt text as the session name.

## License

MIT

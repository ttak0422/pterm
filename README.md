# pterm [![built with nix](https://builtwithnix.org/badge.svg)](https://builtwithnix.org)

Persistent terminal sessions for Neovim.

Processes survive Neovim restarts. Terminal rendering is delegated to Neovim's native terminal (`jobstart(..., { term = true })`) while pterm keeps PTY processes alive.

## CLI

```sh
# Create a new persistent session (forks into background)
pterm new mysession
pterm new mysession --cols 120 --rows 40
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
:Pterm              " opens/creates 'main' session
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
  -- Path to pterm binary (auto-detected if nil)
  binary = nil,
  -- Default shell command
  shell = vim.env.SHELL or "/bin/sh",
  -- Fallback terminal size (Neovim window size takes priority)
  cols = 80,
  rows = 24,
  -- Socket directory (nil = let daemon decide)
  socket_dir = nil,
  -- Max wait time for daemon socket creation after `pterm new`
  attach_wait_ms = 3000,
  -- Poll interval while waiting for socket
  attach_poll_ms = 50,
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

## License

MIT

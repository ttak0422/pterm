# pterm [![built with nix](https://builtwithnix.org/badge.svg)](https://builtwithnix.org)

Persistent terminal sessions for Neovim.

Processes survive Neovim restarts. Terminal rendering is delegated to Neovim's native terminal (`jobstart(..., { term = true })`) while pterm keeps PTY processes alive.

## CLI

```sh
# Create a new persistent session (forks into background)
pterm new mysession

# Attach bridge mode (for terminal clients)
pterm attach mysession

# Attach if exists, otherwise create and attach
pterm open mysession

# List active sessions
pterm list

# Get socket path for a session
pterm socket mysession

# Redraw terminal (resend snapshot to all clients)
pterm redraw mysession

# Kill a session
pterm kill mysession
```

## Neovim Usage

```vim
:Pterm              " opens/creates default session
:Pterm dev          " opens/creates named session
:PtermList          " list sessions
:PtermRedraw dev    " redraw a session
:PtermKill dev      " kill a session
```

`pterm` opens a terminal buffer backed by `jobstart({ "pterm", "attach", <name> }, { term = true })`.

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
  -- Default terminal size
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

## License

MIT

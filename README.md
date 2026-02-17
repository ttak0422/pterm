# pterm

Persistent terminal sessions for Neovim.

Processes survive Neovim restarts. Terminal rendering is delegated to Neovim's native terminal (`jobstart(..., { term = true })`) while pterm keeps PTY processes alive.

## Local Development

### Prerequisites

- [Nix](https://nixos.org/) (with flakes enabled)
- or Rust toolchain + `libiconv` (macOS)

### Build

```sh
# Nix (recommended)
nix build                 # builds Neovim plugin (includes daemon)
nix build .#pterm-daemon  # builds daemon only

# Development shell (provides Rust toolchain)
nix develop
cargo build --release
```

The daemon binary from workspace builds is at `target/release/pterm`.

### Run the daemon CLI

```sh
# Create a new persistent session (forks into background)
./target/release/pterm new mysession

# Attach bridge mode (for terminal clients)
./target/release/pterm attach mysession

# Attach if exists, otherwise create and attach
./target/release/pterm open mysession

# List active sessions
./target/release/pterm list

# Get socket path for a session
./target/release/pterm socket mysession

# Kill a session
./target/release/pterm kill mysession
```

### Environment variables

| Variable | Description |
|---|---|
| `PTERM_SOCKET_DIR` | Override socket directory |
| `SHELL` | Default command if none specified |

## Neovim Usage

```vim
:Pterm              " opens/creates default session
:Pterm dev          " opens/creates named session
:PtermList          " list sessions
:PtermKill dev      " kill a session
```

`pterm` opens a terminal buffer backed by `jobstart({ "pterm", "attach", <name> }, { term = true })`.

## How It Works

- Rust daemon (`pterm new`) owns the child process PTY and stores raw scrollback bytes.
- Neovim opens an attach bridge process (`pterm attach <name>`) as a terminal job.
- The bridge forwards stdin/stdout to the daemon over a framed Unix-socket protocol.
- Neovim/libvterm renders terminal output natively.

This avoids Lua-side terminal byte parsing and improves compatibility with high-refresh TUI apps.

## Session Lifecycle

- Closing/deleting a Neovim buffer only detaches the client; the session continues running.
- A session is deleted by `:PtermKill` / `pterm kill`, or when its socket file is removed externally.
- If the session socket file disappears, the daemon treats the session as deleted and exits.

## Socket Location

Socket root directory is resolved in this order:

1. `$PTERM_SOCKET_DIR`
2. `$XDG_RUNTIME_DIR/pterm`
3. `/tmp/pterm-$UID`

Current session layout is:

```text
<socket_root>/<session_name>/socket
```

Session names may contain `/` for hierarchy, for example:

```text
/tmp/pterm-1000/
├── main/
│   └── socket
└── project/
    ├── socket
    └── build/
        └── socket
```

## Requirements

- Neovim 0.10+
- [Nix](https://nixos.org/) (with flakes enabled), or Rust toolchain
- Linux / macOS

## Install

```lua
-- lazy.nvim
{
  "ttak0422/pterm",
  build = "nix build",
  config = function()
    require("pterm").setup()
  end,
}
```

## License

MIT

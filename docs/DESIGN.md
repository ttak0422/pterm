# pterm - Design

## Overview

pterm provides persistent terminal sessions for Neovim.

- `pterm new` starts a background daemon that owns a PTY + child process.
- Neovim uses `jobstart({"pterm", "attach", name}, { term = true })` to attach.
- `pterm attach` is a bridge process between Neovim terminal PTY and daemon socket.

This architecture keeps process/session persistence in Rust while letting libvterm handle rendering natively.

## Architecture

```text
┌─ Neovim ───────────────────────────────────────────┐
│ Lua plugin                                         │
│ ├─ :Pterm creates/fetches session                  │
│ └─ jobstart({"pterm","attach",name},{term=true})   │
└───────────────────┬────────────────────────────────┘
                    │ terminal PTY (owned by Neovim)
                    ▼
            pterm attach (bridge process)
                    │ Unix socket (framed protocol)
                    ▼
┌────────────────────────────────────────────────────┐
│ pterm daemon                                       │
│ ├─ owns session PTY                                │
│ ├─ stores raw byte scrollback ring                 │
│ ├─ multiplexes multiple clients                    │
│ └─ exits when session socket is removed            │
└───────────────────┬────────────────────────────────┘
                    │
                    ▼
              Child process (shell/TUI)
```

## Why Attach Bridge + `term=true`

Earlier Lua-based rendering (`nvim_open_term` + `nvim_chan_send`) made the Lua scheduler part of the hot path.

Current approach:

- renders through Neovim's native terminal data path
- avoids Lua-side terminal byte parsing
- improves compatibility for high-refresh/full-screen TUIs (btm, htop, tmux, etc.)

## Components

### Lua plugin (`lua/pterm/init.lua`)

Responsibilities:

- binary discovery (`setup.binary` override supported)
- session create/list/kill orchestration
- attach lifecycle in Neovim

Key behavior:

- `:Pterm <name>`:
1. if session socket exists, attach
2. else run `pterm new <name>` and wait for socket, then attach

- attach uses a fresh buffer because `jobstart(..., { term = true })` requires an unmodified current buffer.
- closing/deleting the pterm buffer detaches only; it does not kill the daemon session.

### Daemon (`src/main.rs`, `src/server.rs`, `src/session.rs`)

Responsibilities:

- create and own PTY/child process
- keep process alive independently from Neovim
- store raw output scrollback ring
- serve multiple attach clients over Unix socket

Notable behavior:

- session socket path: `<socket_root>/<session>/socket`
- if socket file is removed externally, daemon treats session as deleted and exits
- output delivery uses per-client send queues and writable polling to avoid disconnecting on backpressure (`WouldBlock`)

### Bridge (`src/bridge.rs`)

Responsibilities:

- stdin -> daemon `INPUT`
- daemon `OUTPUT/SCROLLBACK` -> stdout
- resize propagation (`SIGWINCH` -> daemon `RESIZE`)

Implementation notes:

- nonblocking poll loop (`mio`)
- raw mode guard for terminal settings restoration
- framed protocol parsing with buffered partial-frame handling
- `EINTR` on `poll` is retried

## Wire Protocol

All messages are framed:

```text
[type: u8][length: u32 little-endian][payload bytes]
```

Client -> daemon:

- `INPUT` (`0x01`): raw keyboard bytes
- `RESIZE` (`0x02`): `cols:u16, rows:u16`
- `DETACH` (`0x03`): empty payload

Daemon -> client:

- `OUTPUT` (`0x01`): raw PTY output bytes
- `EXIT` (`0x02`): `exit_code:i32`
- `SCROLLBACK` (`0x80`): full stored scrollback on attach

## Socket and Session Layout

Socket root directory resolution order:

1. `PTERM_SOCKET_DIR`
2. `XDG_RUNTIME_DIR/pterm`
3. `/tmp/pterm-$UID`

Hierarchical sessions are represented by directories:

```text
<root>/project/socket
<root>/project/build/socket
```

## Lifecycle and Deletion Rules

- Detach (buffer close / job stop) does not delete session.
- Session deletion is explicit via `pterm kill` / `:PtermKill`, or by removing the session socket file externally.
- plugin code should not remove socket files automatically.

## Known Limitations / TODO

- persistence across reboot (save/restore scrollback)
- multi-window UX improvements for a single session
- optional health/reconnect diagnostics
- signal forwarding policy beyond raw input (if needed)

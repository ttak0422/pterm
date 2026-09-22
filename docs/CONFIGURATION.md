# Configuration

## Lua options

| Option | Default | Description |
|---|---|---|
| `auto_redraw` | `true` | Automatically redraw on `BufEnter` / `TermEnter` to recover from rendering corruption during mode or window focus switches |
| `auto_redraw_delay_ms` | `1000` | Cooldown window (ms) after an automatic redraw; the first `BufEnter` / `TermEnter` redraw fires immediately and repeated events are suppressed until the cooldown expires |
| `socket_dir` | `nil` | Override socket directory (nil = let daemon decide) |
| `shell` | `$SHELL` or `/bin/sh` | Default shell for new sessions; an explicit command takes precedence |

## Environment variables

| Variable | Description |
|---|---|
| `PTERM_SOCKET_DIR` | (optional) Override socket directory |
| `SHELL` | (optional) Default command if none specified |
| `PTERM_HISTORY_REPLAY_LINES` | Maximum scrollback lines replayed on attach; default is all retained lines (up to 10,000) |

The plugin passes `socket_dir` and `shell` to each pterm subprocess without changing Neovim's global environment.

## Input backpressure

The bridge pauses stdin while at least 64 KiB is waiting to reach the daemon.
The daemon keeps up to 1 MiB of pending PTY input per session. If the child stops
reading and a client exceeds that limit, that client is disconnected; the session
and other clients remain available. Input already accepted stays queued in order.

When stdout stops reading, the bridge queues output and waits for it to become
writable. At 1 MiB of queued output it pauses daemon reads while continuing to
process stdin, socket writes, and resize signals. One decoded frame (up to 64 MiB)
and a 64 KiB socket read can exceed that watermark. Output and terminal cleanup
drain in order before the bridge returns the daemon's exit code. Pipes, PTYs, and
regular-file output are supported; shared file flags and terminal settings are
restored on exit.

## Resource limits

These fixed limits apply to each session/connection; they do not change the wire format.

| Resource | Limit | On excess |
|---|---|---|
| Client request payload | 1 MiB per frame | Disconnect that client as soon as its header arrives |
| Daemon response payload | 64 MiB per frame | Reject the response and close that connection |
| Terminal dimensions | 512 columns, 256 rows, and 65,536 cells | Reject RESIZE before changing the PTY or screen |
| Daemon output queue | 128 MiB + 64 KiB per client | Disconnect that client; keep the session and other clients running |

Receivers decode after every socket read, retaining only a trailing incomplete frame
between reads. An oversized header never causes allocation of its declared payload.
The width limit also bounds each of the 10,000 retained scrollback rows to 512 cells.
The limits bound these resources per connection/session, not total memory across an
unlimited number of clients or sessions.

The output budget accommodates a maximum-sized history frame and snapshot together,
plus handshake/framing overhead. It counts the full buffers of partially sent frames;
accepted frames retain their order and are not truncated to make room. PTY output is
flushed in batches while it is drained so a continuously writing child cannot grow
one pending OUTPUT frame indefinitely.

The payload-size regression fills 10,000 history rows with a different 256-color
foreground on every character. Measured payload sizes in bytes are:

| Screen | Snapshot | History | JSON dump |
|---|---:|---:|---:|
| 80 × 24 | 20,325 | 8,860,048 | 995,814 |
| 240 × 80 | 219,185 | 27,780,160 | 10,282,428 |
| 512 × 128 | 749,085 | 59,020,256 | 35,249,898 |

Exceptionally dense RGB/combining-character history or other state can exceed the
64 MiB response limit. Such responses close the requesting connection without
truncating the data or stopping the session. Lower `PTERM_HISTORY_REPLAY_LINES` in
the daemon's environment before creating a session if a smaller attach replay is needed.

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

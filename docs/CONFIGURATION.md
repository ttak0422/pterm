# Configuration

## Lua options

| Option | Default | Description |
|---|---|---|
| `auto_redraw` | `true` | Automatically redraw on `BufEnter` / `TermEnter` to recover from rendering corruption during mode or window focus switches |
| `auto_redraw_delay_ms` | `1000` | Cooldown window (ms) after an automatic redraw; the first `BufEnter` / `TermEnter` redraw fires immediately and repeated events are suppressed until the cooldown expires |
| `socket_dir` | `nil` | Override socket directory (nil = let daemon decide) |

## Environment variables

| Variable | Description |
|---|---|
| `PTERM_SOCKET_DIR` | (optional) Override socket directory |
| `SHELL` | (optional) Default command if none specified |

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

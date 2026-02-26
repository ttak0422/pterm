# Configuration

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

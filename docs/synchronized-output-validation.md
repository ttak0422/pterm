# Synchronized output coalescing (experimental mitigation)

The bridge now coalesces the canonical seven-bit `CSI ? 2026 h` through
`CSI ? 2026 l` sequence before queueing stdout. This includes legacy OUTPUT,
HISTORY/STATE_SYNC and managed VIEW output after history-reset ordering. State
is local to each bridge; the daemon, session snapshots, and other clients are
unchanged.

This is a mitigation to test, **not a confirmed fix for cursor flicker**. The
working hypothesis is that Neovim 0.12.6 can report intermediate cursor positions
while synchronized text updates are held. The actual bytes associated with the
reported video have not been captured, so causality is not established. A
completed managed VIEW was often already queued as one chunk before this change.
A single stdout buffer is not an atomic PTY write: short writes, OS transport,
and Neovim/libvterm can still split it or report intermediate cursor movement.

## Bounds and compatibility

- A complete supported update is offered together to the existing nonblocking
  stdout queue. Nested begin markers do not extend its deadline.
- Hold for at most 25 ms of intentional waiting, or about 256 KiB (plus at most
  one delimiter). Poll uses the remaining deadline. Scheduling and stdout
  backpressure can delay actual delivery beyond that deadline.
- On either bound, flush and pass through until a closing marker. No bytes are
  inserted, removed, decoded as Unicode, or rewritten, even for malformed input.
- Incomplete delimiter candidates also have the same deadline. A marker split
  across a deadline may pass through without coalescing.
- Other DECSET spellings, including combined parameters and eight-bit CSI, are
  not coalesced. Strings (OSC/DCS/APC/PM/SOS) are not searched for markers.
- Ordinary output has no intentional delay; an incomplete ESC/delimiter prefix
  can wait up to the deadline. Keyboard input and resize handling are not gated.
- On exit, EOF, or detach, held bytes precede the existing terminal cleanup.
  Timeout does not synthesize an END; the receiving terminal still owns its
  own synchronized-update timeout.

## Manual comparison

1. Build this branch (`nix build .#pterm-daemon`) and the unmodified main branch.
   Use a separate test session for each. Confirm the bridge executable actually
   comes from the selected build; do not replace a binary under a live session.
2. In Neovim 0.12.6, repeat the original flicker-producing action with the same
   application, terminal dimensions, cursor settings, and plugin configuration.
   Compare recordings. Include repeated redraws, typing, and rapid window moves.
3. Repeat with small and large terminal dimensions and with a second client
   attached. Resize/focus each client, detach/reattach, then exit the application.
   Check cursor position/visibility, complete text, scrollback, and input latency.
4. If flicker persists, capture the bytes at the bridge stdout/Neovim PTY boundary
   alongside timing and cursor notifications before attributing it to DEC 2026.
   A visual improvement alone does not prove the proposed cause.

Automated tests cover delimiter fragmentation, exact byte preservation,
OSC/DCS-like payloads, incomplete markers, missing END, repeated BEGIN,
time/size fallback, subsequent recovery, short writes/WouldBlock, and a live
bridge timeout with responsive keyboard input. Existing suites cover managed
history reset, multi-client state, resize, detach and output backpressure.

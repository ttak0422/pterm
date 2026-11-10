# Incremental managed-view validation

## Scope and diagnosis

Baseline: main `76663a45224c33f5b420485a6f1b92b96f2c3af7`, including
merged PRs [#102](https://github.com/ttak0422/pterm/pull/102) and
[#104](https://github.com/ttak0422/pterm/pull/104).

PTY reads already drain available bytes before publishing, and Lua does not
replace Neovim buffer lines: the bridge feeds Neovim's native terminal. The
remaining measurable inefficiency was `build_canonical_view`: every flush
cleared the entire native display and repainted every canonical row, even for
a cursor move or an incomplete escape sequence with no visible change.

Managed clients now retain their last **queued** rendered rows and terminal
state. Ordinary updates paint only changed rows; cursor/mode-only updates do
not paint rows; unchanged views produce no frame. Existing synchronized-output
gating, bridge coalescing, PTY polling and input scheduling are unchanged.
There is no added timer or waiting interval.

A complete repaint remains necessary on initial attachment, queue replacement,
physical/canonical geometry changes, alternate-screen changes, history replay
or reset (even zero replay lines), repeated HELLO awaiting acknowledgement,
and explicit REDRAW. REDRAW does not reset history. The cache is per client
and includes both canonical and physical geometry. It retains rendered rows,
not parser scrollback. A partially written frame remains ordered before a
replacement, which does not depend on any discarded delta.

This removes pterm-generated whole-screen blanking from ordinary updates. It
does not promise atomic writes, eliminate application-originated blank frames
without synchronized updates, or optimize scrolling into terminal scroll
commands. A scrolling body still repaints its changed rows.

## Measured results

Linux x86_64 cloud workspace, Rust 1.87, release build. The pre-change renderer
is retained under `cfg(test)` as an independent equivalence and measurement
reference. Measurements below exclude socket, bridge, Neovim, display/GPU and
user-input delivery. `total` includes parser updates, view generation, metric
counting and vt100 replay. Timing is the median of three runs, not an input
latency or FPS claim.

Each synthetic case uses 120 columns × 40 rows and 2,000 updates. The scroll
case scrolls rows 2–39, retaining header/footer. Initial paint is excluded.

| Case | Bytes, before → after | Row paints | Full clears | Total ms | vt100 replay ms |
|---|---:|---:|---:|---:|---:|
| Unchanged | 10,026,000 → 0 | 80,000 → 0 | 2,000 → 0 | 566.0 → 315.2 | 265.4 → 0.1 |
| Cursor only | 10,021,550 → 199,550 | 80,000 → 0 | 2,000 → 0 | 573.9 → 332.2 | 265.7 → 1.9 |
| One changed row | 10,024,000 → 518,000 | 80,000 → 2,000 | 2,000 → 0 | 604.2 → 329.7 | 279.9 → 10.3 |
| Scrolling body | 2,791,488 → 2,297,488 | 80,000 → 76,000 | 2,000 → 0 | 208.9 → 193.4 | 74.2 → 47.9 |

Only the unchanged case reduces frame count (2,000 → 0); the other cases still
emit 2,000 frames. A row repaint is counted by the generated CSI 2 K, and a
full clear by CSI 2 J. This is not a Neovim buffer-update count.

A real **htop 3.4.1** PTY capture at 120×40, with 40 Down inputs then exit,
contained 96 read chunks / 14,934 source bytes. Replaying the same chunks
through both renderers gave:

- 96 views in both paths.
- Generated view bytes: 194,107 → 29,460 (84.8% lower).
- Full clears: 96 → 2 (initial/alternate-screen transition).
- Row paints: 3,840 → 175 (95.4% lower).
- Rendered terminal state, cells/styles, cursor position and visibility matched
  the full renderer after every chunk.

The capture is not a visual flicker test or daemon/Neovim integration test.
Transient local process-list contents are not committed with the fixture.

## Regression coverage and limitations

Passed locally:

- 110 non-socket binary tests, including six new incremental-view tests.
- Two pure server synchronization/gating tests.
- 21 reflow tests and 11 protocol tests.
- One non-socket CLI path-safety test (145 ordinary tests in total).
- Both opt-in measurement tests; htop replay checks every intermediate state.
- `cargo check --locked --workspace --all-targets`, rustfmt, `git diff --check`,
  and Python measurement-script syntax validation.

New coverage includes unchanged/cursor/mode/tracked-title updates, shortened
rows, wide glyphs and styled blanks, scrolling with stable header/footer,
resize/cropping/padding, alternate transitions, history replay and recovery,
independent differently sized clients, and 500 deterministic mixed terminal
operations compared against the original renderer. Existing socket tests were
updated for no-op suppression and extended for explicit REDRAW and repeated
HELLO recovery; they compile but could not execute here.

Not passed/verified:

- Full workspace tests: 12 server cases cannot bind Unix sockets (`EPERM`),
  also with the available elevated-execution route. Socket-dependent CLI
  tests are blocked by the same environment. This is not a full-suite pass.
- Clippy is not installed; Nix, Neovim and tig are unavailable locally.
- Actual tig scrolling, htop visual flicker, Neovim `:terminal` comparison,
  native shell/input latency, and live resize/focus/backpressure integration
  require an unrestricted host with the pinned Neovim version.

The visual acceptance condition and real input-response condition remain open.
No PR, installation, live session restart or configuration change is part of
this local validation.

## Reproduction

```sh
cargo test --locked --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
nix flake check
cargo test --release --locked --bin pterm measure_incremental_view_updates -- --ignored --nocapture
```

For the real daemon/PTY protocol benchmark, build the baseline and changed
executables separately, then run on a host allowing Unix sockets:

```sh
python3 tests/measure_managed_views.py /path/to/baseline/pterm /path/to/changed/pterm
```

The script reports wire bytes, VIEW count, clear/row-paint count and the
input-to-first-VIEW time. Its fixed receive window is measurement pacing, not
input delay. It does not measure pixels. Execution here reached the daemon
startup timeout, so no daemon-protocol timings are reported above.

A private 120×40 TUI capture can also be replayed with the opt-in
`measure_captured_tui_updates` test. Set `PTERM_TUI_CAPTURE` to a file containing
repeated little-endian u32 chunk lengths followed by the original PTY bytes.
The test compares each resulting state against the full renderer. It does not
reconstruct source timing or daemon history-generation decisions.

On the unrestricted host, compare identical tig continuous-scroll and htop
operations in baseline pterm, changed pterm and native `:terminal`. Record
flicker and input response, then run the existing real Neovim resize/tig
regressions. Include shell typing, cursor movement, rapid focus/resize,
differently sized clients, detach/reattach, application exit, explicit redraw,
and stopped-output/backpressure cases before declaring visual completion.

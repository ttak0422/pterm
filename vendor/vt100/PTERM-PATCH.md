# pterm's vt100 patch

Vendored from https://github.com/doy/vt100-rust, tag `v0.16.2`, commit
`eb66ffaf7d771c13303ef73b29f6f2a56fdacecf`, matching pterm's previous lockfile.
The upstream MIT license is retained in LICENSE. Runtime dependencies are
unchanged; upstream example/test-only dev-dependencies are omitted.

Local changes:

- Reflow the primary grid and history directly in `Screen::set_size`, retaining
  cell attributes, Unicode combining contents, cursor anchors, the hidden
  primary screen during alternate-screen use, and the parser's in-flight state.
  Alternate-screen resizing remains grid-based.
- Retain history by meaningful natural-width cells and logical line endings,
  rather than physical row count. The combined live/history cell budget is `(configured history +
  largest viewport height) * largest viewport width`; the viewport reserve
  prevents narrowing a full history buffer from evicting newly displaced live
  content. There is also a logical-line limit of configured history plus that
  viewport reserve. Widths/heights only increase these budgets, never shrink
  them. A configured history of zero remains zero. One-column history can
  have more row allocations, but both row count and content remain bounded.
- Preserve wide Unicode in one-column grids as one storage cell and restore
  natural width when widened. Preserve artificial padding at wide-glyph wraps.
  Repair the upstream one-row wrapping index underflow and alternate-grid
  orphaned wide cells after shrinking.
- Expose primary `scrollback_generation()` and `scrollback_rows()`. Generation
  increments on output-driven archival, not reflow; consumers reset their
  baseline on resize or terminal reset. A terminal reset resets the archive
  counter and increments the independent `reset_generation()` epoch, including
  resets followed by new output within a single parser input batch.
- Expose `Cell::natural_width()` so a clipped renderer can show a placeholder
  for compressed wide glyphs without discarding their original contents.

Cursor anchors that cannot fit the visible viewport are displayed clamped to
its edge and retained across further resizes. Actual drawing invalidates hidden
anchors because subsequent terminal editing addresses the visible grid.

Regression tests live in `src/reflow_tests.rs` and use only runtime dependencies.

Additional narrowly scoped compatibility fixes:

- Retain the natural within-glyph offset of both cursor anchors while a wide
  glyph is compressed, including a cursor originally on its continuation cell.
- Honor DECAWM (`CSI ? 7 h/l`), default/reset enabled, with right-margin
  overwriting when disabled. A wide glyph clipped at a no-wrap margin retains
  its Unicode in one storage cell for the renderer to replace visually.
  Expose `Screen::autowrap()` and preserve the mode in state snapshots and
  DEC cursor save/restore.

This is still vt100's deliberately incomplete terminal model. Notably, its
upstream dispatcher does not implement ANSI insert/newline modes (`CSI 4 h/l`,
`CSI 20 h/l`), HVP (`CSI f`), ANSI cursor-save/restore aliases (`CSI s/u`), or
repeat-character (`CSI b`). This patch does not add those unrelated features.

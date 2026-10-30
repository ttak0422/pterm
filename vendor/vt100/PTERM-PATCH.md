# pterm's vt100 patch

Vendored from https://github.com/doy/vt100-rust, tag `v0.18.1`, commit
`fb0f54ec7222f4805f33cd3ca530229e0a0fe6ad` (2026-10-19).
This ports the pterm patches previously based on `v0.16.2` /
`eb66ffaf7d771c13303ef73b29f6f2a56fdacecf`.
The upstream MIT license is retained in LICENSE. Runtime dependencies are
unchanged from upstream 0.18.1; upstream example/test-only dev-dependencies are
omitted. The optional embedded-io feature and default std feature are retained.
The new upstream edition 2024 / Rust 1.85 minimum fits the existing Rust 1.98.1
toolchain; no toolchain or unrelated dependency update is included.

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
  Retain pterm's one-row wrapping and alternate-grid wide-cell repairs while
  preserving upstream's new one-row scroll-region handling and VS16 widening.
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

Upstream changes retained in this update:

- NonZeroU16 geometry, UTF-8 String formatted output, no_std/embedded-io,
  basic-formatted capture, on_scroll callbacks and Parser::set_size.
- DECSCUSR, HPA/HVP/REP, CSI 3 J / clear_scrollback and VS16 emoji width.
- Scroll-region/scrollback-offset reset on resize, and IL/DL no-ops outside the
  region. Alternate-screen height changes follow upstream; the primary grid
  deliberately retains pterm logical reflow rather than upstream truncation.
- Eviction callbacks fire once per discarded row, including CSI 3 J. Reflow
  alone does not represent eviction. Wrapped archived rows retain padding and
  wrap metadata; only unwrapped archived rows trim default trailing cells.

Screen::set_size skips callbacks as upstream documents; Parser::set_size
uses the same primary reflow and forwards eviction callbacks. Clearing history
increments pterm's reset epoch so managed clients discard stale rendered history.
Reflow does not synthesize upstream's theoretical lost-history blank rows: pterm
retains actual logical contents and cursor anchors within its bounded budgets.

pterm callers now use upstream cursor_style() for snapshots, canonical views and
the existing numeric cursor_shape dump field (default is explicitly 0). They no
longer rely on unhandled_csi for DECSCUSR. The wire protocol and dump schema stay
unchanged. Unsupported cursor-style numbers follow upstream's handling.

This remains an incomplete terminal model. ANSI insert/newline modes (`CSI 4 h/l`,
`CSI 20 h/l`) and ANSI cursor-save/restore aliases (`CSI s/u`) remain outside this
patch. The earlier audit saying upstream was still 0.16.2 was incorrect; the
baseline above is verified from the official 0.18.1 tag and source manifest.

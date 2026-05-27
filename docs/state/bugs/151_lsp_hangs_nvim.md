# Bug 151 — LSP rebuild hangs nvim

**Status:** open  **Severity:** medium  **First seen:** 2026-05-28
**Repro reliability:** observed on user's setup; not reproduced in
synthetic LSP protocol tests.

After running `cargo install --path . --force` on `lake-lsp`, nvim
freezes completely when opening a `.lake` file from `lake-server/`.
Full nvim freeze — keystrokes do nothing, must kill the terminal.

Rolling lake-lsp back to a binary built against an older frontend
(`8bced90 rt_registry: register rt_arena_alloc`) did **not** fix the
hang.  So the regression source is not (only) the front-end changes
introduced by `384e99a` + `99d0fb6` + `a73d19e` + `49ee343`.

Workaround: `mv ~/.cargo/bin/lake-lsp ~/.cargo/bin/lake-lsp.disabled`
to disable LSP entirely until root-caused.

## What was tested

Synthetic LSP protocol over stdin/stdout (`python` driver, see
`/tmp/lsp_*.py` scratchpads in session 949499ad) covering:

* `initialize` → `initialized` → `didOpen(lake-server/main.lake)`
  with bare-bones, semanticTokens-aware, and full nvim-style
  capability sets.  All finished in ≤ 0.2 s with
  `Analyzed: 211 symbols, 98 machines, 22 runtime funcs`.
* `didChange` storm — 100 events in 2 s, each followed by a
  `completion` request.  Server stayed responsive
  (`id_999` round-trip in ≤ 3 s).
* `semanticTokens/full` after `didOpen` — replied in 0.20 s.

None of these reproduced the hang.

## What still leaks suspicion

* `analyze_document` in `lake-lsp/src/main.rs:405` calls
  `load_and_build(&path)` on every `didOpen` and (via
  `did_change` → same path) every `didChange`.  No debouncing.
  For lake-server's import graph (main.lake → http.lake →
  stdlib.tcp/io/strings/bytes/...) that's ~666 ms / refresh per
  `lakec -O speed`.  Under rapid typing the queue saturates the
  tokio runtime; nvim's LSP client may block its main loop
  waiting on a response that's stalled behind several seconds of
  queued work.
* Tower-lsp's default dispatch spawns one task per request but
  `documents: DashMap` writes inside `analyze_document` serialize.
  Lock-contention with many concurrent analyses might starve the
  response loop.

## Next steps

1. Reproduce in a minimal nvim setup (clean config + `lake-lsp`
   only) on `lake-server/main.lake`.
2. Add a debounce on `did_change` (drop stale analyses when a
   newer version arrives).
3. Split `analyze_document` into a single-file fast pass (for
   per-keystroke updates) + an opt-in slow multi-file pass
   (on save or after debounce).

## Related

* `7529386 feat: multi-file pipeline` introduced the
  `load_and_build` path on every `analyze_document`; the per-edit
  cost has grown with every stdlib expansion since.
* `384e99a` + enum phases shipped immediately before the hang
  was first observed; they aren't the proven cause but stay on
  the suspect list until reproduced.

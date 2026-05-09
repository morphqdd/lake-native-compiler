# Canonical task benchmarks

Productivity / expressiveness comparison: same task implemented in each
language, compared by **lines of source that carry meaning** (blank
lines and single-line comments stripped).

This axis does **not** measure speed.  See `perf/` for that.

## Tasks

| Task          | Status | Demonstrates                          |
|---            |---     |---                                    |
| fizzbuzz      | done   | basic control flow + I/O              |
| fib_actor     | done   | actor model + tail recursion          |
| ping_pong     | done   | mailbox-style message passing         |
| echo_server   | done   | TCP listen/accept/send/close          |
| word_count    | partial — Lake blocked | stdin streaming I/O |

Lake's `word_count` is gated on `rt_read` returning the read byte count
(currently void).  Once that's fixed it can be implemented in a handful
of lines using `rt_load_u8` and a self-recursive counter — see the
commented body in `word_count/lake.lake`.

## Layout

Each task lives in `canonical/<task>/` with one source file per
language plus a `manifest.sh` describing the task:

```
canonical/<task>/
  manifest.sh            # NAME, DESC, LANGS
  lake.lake
  go.go
  cpp.cpp                # optional
  c.c                    # optional
  rust/
    Cargo.toml
    src/main.rs
```

## Harness

`./benchmark/run.sh canonical` invokes `_axis.sh`, which:

1. Walks each task directory.
2. Counts effective LoC for each language source (skips blanks, `//`
   line comments, and shell `#` comments).
3. Writes a side-by-side table to `benchmark/results/canonical.md`
   and prints a per-task summary.

Filter to one task:

```bash
./benchmark/run.sh canonical fizzbuzz
```

# Canonical task benchmarks

Productivity / expressiveness comparison: same task implemented in each
language, compared by:

- **Lines of code** (excluding blank lines and comments)
- **Boilerplate ratio** (ceremony chars / business-logic chars)
- **Source size in bytes**

## Tasks (TODO — none implemented yet)

| Task | Status | Demonstrates |
|---|---|---|
| fizzbuzz       | TODO | basic control flow + I/O |
| fib_actor      | TODO | one actor computes fib(N) and prints |
| ping_pong      | TODO | two actors exchanging N round-trips |
| echo_server    | BLOCKED | needs sockets / `rt_listen` / `rt_accept` |
| word_count     | BLOCKED | needs file I/O / `rt_open` |

Each task lives in `canonical/<task>/` with one file per language
(`lake.lake`, `cpp.cpp`, `go.go`, `rust/`).

The harness for this axis is **not yet implemented**. When ready it will:

1. `wc -l --no-blank --no-comment` each file
2. produce `results/canonical-loc.md` with a side-by-side table
3. surface counts in the summary

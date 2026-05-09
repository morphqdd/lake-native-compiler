# Lake — Roadmap

Runtime scope frozen as of commit `fbaba95` (O(1) wake + backlog 4096 +
io_parked cap 4096). Focus shifts to frontend, stdlib, HTTP, and a
production-grade benchmark suite.

End goal: real-time graphs (rps / p50 / p95 / p99 / memory) under
sustained HTTP load, comparable to channels like Anton Putra's
language-vs-language performance series — but with Lake as a peer.

---

## Phase 1 — Frontend cleanup

Close the gap between what the parser/typer accepts and what the
runtime can already execute.

- [ ] Literal-guard type loss in `when` (string/bool patterns)
- [ ] Unary minus parsed as infix
- [ ] `//` comment edge cases (EOF, after expr)
- [ ] `when` over `let str` binding
- [ ] Re-enable 3 ignored integration tests
- [ ] Module system: `import`, multi-file projects, std path resolution

**Exit criteria:** all integration tests pass; multi-file compilation
works; stdlib can be organized as separate modules.

---

## Phase 2 — Stdlib core

Pure-Lake modules, thin wrappers over existing rt funcs where needed.

- [ ] `std/string` — concat, len, slice, parse_int, split, index_of
- [ ] `std/list` — cons, head, tail, length, reverse, map, filter, fold
- [ ] `std/io` — print, println, read_line
- [ ] `std/result` — ok/err, map, and_then
- [ ] `std/option` — some/none, unwrap_or, map
- [ ] `std/net` — listen, accept, recv, send, close (wrappers over rt_*)
- [ ] `std/time` — sleep_ms (parking), now_ms

**Exit criteria:** counter / ping_pong / sum examples rewritten using
stdlib; TCP echo example uses `std/net`.

---

## Phase 3 — HTTP in Lake

Pure-Lake HTTP/1.1 (no C dependency).

- [ ] HTTP/1.1 request parser (request line + headers + body)
- [ ] Response builder (status + headers + body)
- [ ] `std/http` — `server`, `request`, `response` types
- [ ] Minimal routing (path → handler actor)
- [ ] Keep-alive support
- [ ] Chunked transfer encoding (optional)

**Exit criteria:** `examples/http_hello.lake` serves "hello world" on
:8080; passes basic curl tests; handles 1k concurrent connections.

---

## Phase 4 — Production bench suite

Real-time graphs under sustained load. Match the methodology of
modern language benchmark videos (Anton Putra, TechEmpower-style).

### Tooling

- [ ] **wrk2** for constant-rate load (HDR histogram, no coordinated omission)
- [ ] Per-second CSV emission (rps, p50, p95, p99, memory, CPU)
- [ ] gnuplot time-series scripts:
  - rps over time
  - p50/p95/p99 stacked lines
  - latency distribution heatmap
  - memory growth curve

### Scenarios

- [ ] Constant rate (sustained throughput, find ceiling)
- [ ] Ramp up (linear rps increase, find breaking point)
- [ ] Spike test (10× burst, recovery time)
- [ ] Sustained 30 min (leak detection, GC-pause comparison)

### Peers

- [ ] Go net/http
- [ ] Rust axum (tokio)
- [ ] Node express
- [ ] Bun.serve
- [ ] C + nginx (or raw libuv)
- [ ] Java Spring (optional, for JVM comparison)

### Deliverables

- [ ] `benchmark/perf/http/` directory with all peers + load gen
- [ ] `docker-compose.yml` for reproducibility
- [ ] Auto-generated graphs committed per release
- [ ] README with methodology, hardware spec, kernel version

---

## Phase 5 — Thesis writing (parallel)

Write while building. Concrete material from Phases 1-4 backs the
empirical evaluation chapter.

- [ ] Ch.1 — Introduction: actor model, BEAM heritage, design goals
- [ ] Ch.2 — Architecture: scheduler, allocator, ExecCtx/ProcessCtx, CPS
- [ ] Ch.3 — Per-block fairness as novel contribution
- [ ] Ch.4 — io_uring integration: park/wake, syscall folding
- [ ] Ch.5 — Empirical evaluation: density, TCP, HTTP under sustained load
- [ ] Ch.6 — Comparison: Lake vs C / Rust+tokio / Go / Node / Bun
- [ ] Ch.7 — Limitations and future work

---

## Selling points to highlight in final graphs

- **Flat latency line** — no GC pauses (visual win vs JVM / Go)
- **Binary size** — Lake 12 KB vs Go ~8 MB vs Java ~200 MB
- **Memory per connection** — few KB per parked actor
- **Cold start** — instant vs JVM warm-up
- **Sustained 30 min without degradation**
- **Source line count** — Lake server in 17 lines (vs equivalents)

---

## Out of scope (deferred)

- Multi-threaded scheduler (year+ of work)
- Distributed Erlang-style clustering
- Supervisor trees (after stdlib is stable)
- Hot code reload
- Multishot accept with per-actor CQE queue
- io_parked dynamic grow path (currently capped at 4096)
- Fixed buffers / SQPOLL io_uring optimizations

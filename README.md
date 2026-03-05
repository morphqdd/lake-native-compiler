# Lake

Lake is a process-oriented systems programming language that compiles to native x86-64 binaries via [Cranelift](https://cranelift.dev/). Every function call is a process spawn. There is no explicit `async`/`await` — concurrency is the default.

```lake
@rt(rt_write)

worker is {
  n str -> {
    rt_write(1 n 7)
  }
}

main is {
  n i64.0 -> {
    worker("task 0\n")
    worker("task 1\n")
    worker("task 2\n")
  }
}
```

All three `worker` calls spawn independent processes scheduled cooperatively. No threads. No async runtime boilerplate.

---

## Core Concepts

### Machines and Branches

A *machine* is a named set of pattern-matched branches. Each branch is selected at runtime based on the types (and optionally values) of its arguments:

```lake
handler is {
  n str b str -> { ... }   -- branch 1: two strings
  n str b i64 -> { ... }   -- branch 2: string + integer
}
```

Dispatch is O(1): argument types are hashed at compile time to a 64-bit key, resolved via a registry `HashMap`.

### Every Call is a Spawn

Calling a machine does not transfer control — it spawns a new process and returns immediately. The scheduler runs all live processes cooperatively, interleaving them at block boundaries.

```lake
main is {
  n i64.0 -> {
    worker("task 0\n")   -- spawns process 1
    worker("task 1\n")   -- spawns process 2, main continues
  }
}
```

### Runtime Functions (`@rt`)

Functions marked with `@rt` are direct calls — no spawn. They map to runtime primitives or syscall wrappers and execute inline within the current process:

```lake
@rt(rt_write)
@rt(rt_allocate)
@rt(rt_store)
@rt(rt_load_u64)
```

### Default Parameter Values

Branch parameters can carry default values, enabling zero-argument entry points:

```lake
main is {
  n str."Hello, world!\n" -> {
    worker(n)
  }
}
```

---

## Performance

### I/O benchmark (10 concurrent tasks, write to stdout)

| Runtime         | Time      | vs Lake |
|-----------------|-----------|---------|
| **Lake**        | 288 µs    | 1.0×    |
| Rust (Tokio)    | 1087 µs   | 3.8×    |
| C++ coroutines  | 1769 µs   | 6.1×    |

### CPU benchmark (8 workers, fib(100k), single-threaded)

| Runtime                    | Time      | vs C   |
|----------------------------|-----------|--------|
| C sequential (baseline)    | 729 µs    | 1.0×   |
| Go (GOMAXPROCS=1)          | 2136 µs   | 2.9×   |
| C++ coroutines             | 2282 µs   | 3.1×   |
| Rust (Tokio current_thread)| 3356 µs   | 4.6×   |
| **Lake** (quantum=256)     | 26654 µs  | 36.6×  |

Lake's I/O performance leads because the scheduler operates on atomic blocks — it can preempt a process without explicit `await` points. The CPU gap is architectural: CPS dispatch per iteration provides reduction counting (like BEAM), trading raw throughput for fairness. Optimization trend: 640ms → 260ms → 28ms across iterations.

---

## Ecosystem

| Crate | Role |
|-------|------|
| [`lake-native-compiler`](.) | Compiler: Cranelift codegen, scheduler, linker integration |
| [`lake-frontend`](https://github.com/morphqdd/lake_frontend) | Parser and AST — reusable for linters, formatters, LSP servers |

The frontend is intentionally decoupled from the compiler. Building a linter, formatter, or language server only requires `lake-frontend`.

---

## Building

**Requirements:**
- Rust (edition 2024)
- [`mold`](https://github.com/rui314/mold) linker

```sh
git clone https://github.com/morphqdd/lake-native-compiler
cd lake-native-compiler
cargo build --release
```

---

## Usage

```sh
# Compile a .lake file
cargo run --release -- examples/simple/simple.lake

# Run the resulting binary
./examples/simple/build/simple
```

The compiler writes the object file and calls `mold` to produce a native ELF binary. No libc dependency.

---

## Architecture

```
lake-frontend          →   AST
        ↓
  compiler/pipeline    →   Cranelift IR (one Cranelift function per machine)
        ↓
  compiler/rt          →   Runtime layout (ExecCtx, fat pointers, scheduler)
        ↓
  Cranelift            →   x86-64 object file
        ↓
  mold                 →   native ELF binary
```

**ExecCtx** (40 bytes per process):

| Field       | Offset | Description                         |
|-------------|--------|-------------------------------------|
| `BRANCH_ID` | 0      | Which branch to execute             |
| `BLOCK_ID`  | 8      | Current block within the branch     |
| `TEMP_VAL`  | 16     | Scratch register for rt return values |
| `VARIABLES` | 24     | Fat pointer to process-local variables |
| `JUMP_ARGS` | 32     | Fat pointer to call argument staging buffer |

Each block is a Cranelift function that returns the next `block_id`. The scheduler dispatches blocks via a `Switch` table — O(1) per step.

---

## Roadmap

- [x] Arithmetic operators (`+`, `-`, `*`, `/`)
- [x] `when` expressions (conditional branching)
- [x] `self()` state transitions
- [x] Process spawning and cooperative scheduling
- [x] Quantum batch scheduling (configurable reduction limit)
- [ ] Comparison operators (full set)
- [ ] Process IDs and message passing (`send` / `receive`)
- [ ] `wait` — blocking receive
- [ ] User-defined structs
- [ ] Arena allocator per process
- [ ] `io_uring` integration for async I/O
- [ ] Thread pool for blocking `@rt` calls
- [ ] Multi-file compilation and imports
- [ ] Standard library (file I/O, networking, timers)

---

## License

MIT

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

Calling a machine does not transfer control — it spawns a new process and returns a `pid` (process identifier). The scheduler runs all live processes cooperatively, interleaving them at block boundaries.

```lake
main is {
  n i64.0 -> {
    let p1 pid = worker("task 0\n")   -- spawns process 1, returns pid
    let p2 pid = worker("task 1\n")   -- spawns process 2, returns pid
    p1(42)                            -- send message to p1
  }
}
```

### Message Passing

Processes communicate via message passing. Each process has a mailbox (ring buffer of 256 slots). Calling a `pid` variable sends a message:

```lake
receiver is {
  _ i64.0 -> {
    wait { n i64 -> { rt_write(1 "got message\n" 12) } }
  }
}

main is {
  _ i64.0 -> {
    let p pid = receiver()
    p(42)                 -- send 42 to receiver's mailbox
  }
}
```

The `wait` expression suspends the process until a message arrives. If the mailbox is empty, the process moves to the scheduler's wait array and is awakened when another process sends a message.

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

### Records

A *record* groups named fields into a nominal type. Construct positionally, read with dot-access, and destructure with a `let` pattern:

```lake
Greeting is {
  who   buf
  count i64
}

main is {
  _ -> {
    let g = Greeting("world" 3)
    let { who count } = g
  }
}
```

### Enums

An *enum* is a tagged union of named variants, matched with `when`. Matches must be exhaustive:

```lake
Result is enum {
  Ok(i64)
  Err(buf)
}

unwrap is {
  r Result -> ret i64 {
    when r {
      Ok(n)  -> { ret n }
      Err(_) -> { ret 0 - 1 }
    }
  }
}
```

### Generics

Records, enums, and machines can take type parameters in square brackets. Instantiations are monomorphized — specialized at compile time, no runtime cost:

```lake
Option[T] is enum {
  Some(T)
  None
}

unwrap_or[T] is {
  o Option[T] d T -> ret T {
    when o {
      Some(v) -> { ret v }
      None    -> { ret d }
    }
  }
}
```

---

## Standard Library

The standard library (`lake-stdlib`) is written in Lake itself and imported with `+module.{ names }`:

```lake
+std.io.{ println }
+std.vec.{ vec_new vec_push vec_get vec_len }
```

| Module | Provides |
|--------|----------|
| `std.io` | `print` / `println` / `eprintln` (+ `_buf` variants) |
| `std.option` | `Option[T]` + `is_some` / `is_none` / `unwrap_or` |
| `std.result` | `Result[T E]` + `is_ok` / `is_err` / `unwrap_or` |
| `std.vec` | `Vec[T]` growable vector |
| `std.string` / `std.strings` | owned `String` + string/`buf` algorithms |
| `std.hashmap` | `IntMap[V]` integer-keyed hash map |
| `std.bytes` | bounded byte access over a `buf` |
| `std.math` | `abs` / `mod` / `min` / `max` |
| `std.process` / `std.sys` / `std.env` | allocation, `exit`, argv/envp |
| `std.tcp` | `listen` / `accept` / `send` / `recv` / `close` |
| `std.crypto.*` | `sha256`, `hmac_sha256`, `pbkdf2_sha256` |
| `std.encoding.base64` | `encode` / `decode` |
| `std.postgres` | PostgreSQL client (see below) |

Collections (`Vec`, `String`, `IntMap`) are value-threaded: a mutating call returns a possibly-new value that you bind and reuse.

```sh
# Point the compiler at the stdlib's std/ directory
STD_PATH=/path/to/lake-stdlib/std lakec -O speed main.lake -o out
```

---

## Performance

Benchmarks run on x86-64 with full CPU frequency (single-threaded, `GOMAXPROCS=1` for Go).

### I/O benchmark (10 workers × 10k writes)

| Runtime         | Time      | Relative |
|-----------------|-----------|----------|
| C++ coroutines  | 21.3 ms   | 1.0×     |
| **Lake**        | **22.7 ms** | **1.07×** |
| Rust (Tokio)    | 35.1 ms   | 1.65×    |
| Go              | 43.7 ms   | 2.05×    |

### Message passing (ping-pong 100k round-trips)

| Runtime         | Time      | Relative |
|-----------------|-----------|----------|
| C++ coroutines  | 2.4 ms    | 1.0×     |
| **Lake** (mailbox) | **21.7 ms** | **9.2×** |
| Rust (tokio mpsc) | 24.9 ms  | 10.5×    |
| Go (channels)   | 35.5 ms   | 15.0×    |

### CPU benchmark (8 workers, fib(100k))

| Runtime                    | Time      | vs C   |
|----------------------------|-----------|--------|
| C sequential (baseline)    | 772 µs    | 1.0×   |
| Go (GOMAXPROCS=1)          | 1.8 ms    | 2.4×   |
| C++ coroutines             | 2.1 ms    | 2.7×   |
| Rust (Tokio current_thread)| 3.3 ms    | 4.3×   |
| **Lake** (fused self-call) | **7.0 ms** | **9.1×** |

Lake's I/O performance is competitive because the scheduler operates on atomic blocks — it can preempt a process without explicit `await` points. Message passing beats both Rust and Go thanks to inline mailbox operations and direct process wake-up. The CPU gap is architectural: CPS dispatch + reduction counting (like BEAM) trades raw throughput for fairness and cooperative scheduling guarantees.

---

## Ecosystem

| Crate | Role |
|-------|------|
| [`lake-native-compiler`](.) | Compiler: Cranelift codegen, scheduler, linker integration |
| [`lake-frontend`](https://github.com/morphqdd/lake_frontend) | Parser and AST — reusable for linters, formatters, LSP servers |
| `lake-stdlib` | Standard library, written in Lake (collections, crypto, networking) |

The frontend is intentionally decoupled from the compiler. Building a linter, formatter, or language server only requires `lake-frontend`.

### PostgreSQL client

`std.postgres` is a from-scratch PostgreSQL client written in Lake: it speaks the wire protocol over a raw socket, authenticates with SCRAM-SHA-256, and uses binary codecs for typed columns. The surface is `connect` / `query` / `prepare` / `execute` / `close`:

```lake
+std.postgres.{ connect close query ip_v4 row_field }
+std.postgres.types.{ dec_int4 }

main is {
  _ -> {
    let c = connect(ip_v4(127 0 0 1) 5432 user db password)
    when c.ok {
      1 -> {
        let rows = query(c "SELECT 1::int4")
        close(c)
      }
      _ -> { }
    }
  }
}
```

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

**ExecCtx** (64 bytes per process):

| Field       | Offset | Description                         |
|-------------|--------|-------------------------------------|
| `BRANCH_ID` | 0      | Which branch to execute             |
| `BLOCK_ID`  | 8      | Current block within the branch     |
| `TEMP_VAL`  | 16     | Scratch register for rt return values |
| `VARIABLES` | 24     | Fat pointer to process-local variables |
| `JUMP_ARGS` | 32     | Fat pointer to call argument staging buffer |
| `MAILBOX_FAT` | 40   | Fat pointer to ring buffer (256 × 8 bytes) |
| `MAILBOX_HEAD` | 48  | Read index (consumer)               |
| `MAILBOX_TAIL` | 56  | Write index (producer)              |

Each block is a Cranelift function that returns the next `block_id`. The scheduler dispatches blocks via a `Switch` table — O(1) per step. Messages are enqueued/dequeued from the per-process mailbox with mod-256 wrapping.

---

## Roadmap

- [x] Arithmetic operators (`+`, `-`, `*`, `/`)
- [x] Comparison operators (`<=`, `>=`, `==`, `<`, `>`)
- [x] `when` expressions (conditional branching)
- [x] `self()` state transitions
- [x] Process spawning and cooperative scheduling
- [x] Quantum batch scheduling (configurable reduction limit)
- [x] Process IDs (`pid` type) and message passing
- [x] `wait` expression — blocking receive with mailbox
- [x] Fused self-call optimization (pure args bypass staging)
- [x] Bitwise / shift operators (`&`, `|`, `^`, `<<`, `>>`)
- [x] Byte-string (`b"..."`) and char (`'x'`) literals
- [x] Records (named fields, dot-access, destructure)
- [x] Enums (tagged unions, exhaustive `when` matching)
- [x] Generics (monomorphized records / enums / machines)
- [x] `@cfg(arch=...)` per-target item filtering
- [x] Per-process arena allocator
- [x] Multi-file compilation and imports
- [x] Standard library (collections, strings, crypto, networking)
- [x] `io_uring`-backed async I/O (TCP)
- [x] PostgreSQL client (wire protocol, SCRAM-SHA-256, binary codecs)
- [ ] Protocols / bounded type parameters (`X is Eq Show`, `[T: Proto]`)
- [ ] `panic` / structured error propagation
- [ ] `io_uring` fallback path for non-supporting kernels
- [ ] Thread pool for blocking `@rt` calls
- [ ] Fully generic-key hash map (string / `buf` keys)
- [ ] Modulo (`%`) and `!=` operators

---

## License

MIT

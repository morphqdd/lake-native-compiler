# Lake examples

Each file is a self-contained, runnable program demonstrating one language
feature. Compile and run with:

```sh
cargo run --release -- examples/hello.lake
./examples/hello
```

| File                | Demonstrates                                           |
|---------------------|--------------------------------------------------------|
| `hello.lake`        | minimal program: `@rt`, `main`, `rt_write`             |
| `counter.lake`      | self-recursion + `when` for termination                |
| `sum.lake`          | accumulator pattern via multi-arg `self(...)`          |
| `ping_pong.lake`    | spawning multiple processes, no message passing        |
| `when.lake`         | `when` over numeric discriminant                       |
| `when_string.lake`  | `when` over string discriminant (MPHF dispatch)        |
| `stdin_reader.lake` | advanced: `rt_read`, `rt_allocate`, `wait`/PID coordination |

For runnable test cases (not demos), see `tests/integration/`.

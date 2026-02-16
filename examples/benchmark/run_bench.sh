set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPILER="$SCRIPT_DIR/../../target/release/lake-native-compiler"

echo "=== Building ==="

# Lake
echo "  [lake] compiling bench.lake..."
"$COMPILER" "$SCRIPT_DIR/bench.lake"

# Rust
echo "  [rust] cargo build --release..."
cd "$SCRIPT_DIR/async_rust"
cargo build --release -q
cd "$SCRIPT_DIR"

# C++
echo "  [c++]  clang++ -O2 -std=c++20..."
clang++ -O2 -std=c++20 "$SCRIPT_DIR/async_cpp.cpp" -o "$SCRIPT_DIR/build/async_cpp_bench"

echo ""
echo "=== Benchmark (hyperfine) ==="
hyperfine \
    --warmup 10 \
    --shell none \
    --export-markdown "$SCRIPT_DIR/results.md" \
    --command-name "lake (cooperative, direct syscalls)" \
        "$SCRIPT_DIR/build/bench" \
    --command-name "rust (tokio current_thread)" \
        "$SCRIPT_DIR/async_rust/target/release/bench" \
    --command-name "c++ (coroutines, manual scheduler)" \
        "$SCRIPT_DIR/build/async_cpp_bench"

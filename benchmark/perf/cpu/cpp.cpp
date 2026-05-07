#include <coroutine>
#include <queue>
#include <unistd.h>

// CPU-bound benchmark: 8 coroutines, fib(100000).
//
// Emulates Lake's block model: co_await on every iteration ("block"),
// but the scheduler only actually suspends every 256 resumes (quantum=256).
// await_ready() returns true (skip suspend) until quantum exhausted —
// same overhead pattern as Lake's per-block CPS dispatch.

static std::queue<std::coroutine_handle<>> ready;
static int quantum_ctr = 0;
static constexpr int QUANTUM = 256;

struct Block {
    bool await_ready() noexcept {
        if (++quantum_ctr < QUANTUM) return true; // stay in current coroutine
        quantum_ctr = 0;
        return false; // actually suspend, round-robin
    }
    void await_suspend(std::coroutine_handle<> h) noexcept { ready.push(h); }
    void await_resume() noexcept {}
};

struct Task {
    struct promise_type {
        Task get_return_object() noexcept {
            return Task{std::coroutine_handle<promise_type>::from_promise(*this)};
        }
        std::suspend_always initial_suspend() noexcept { return {}; }
        std::suspend_never  final_suspend()   noexcept { return {}; }
        void return_void()         noexcept {}
        void unhandled_exception() noexcept {}
    };
    std::coroutine_handle<> handle;
};

static void spawn(Task t) { ready.push(t.handle); }
static void run_scheduler() {
    while (!ready.empty()) {
        auto h = ready.front();
        ready.pop();
        h.resume();
    }
}

Task worker() {
    long a = 0, b = 1;
    for (long i = 0; i < 100000; ++i) {
        long tmp = a + b; a = b; b = tmp;
        co_await Block{}; // one "block" per iteration
    }
    volatile long r = b; (void)r;
    write(1, ".\n", 2);
    co_return;
}

int main() {
    for (int i = 0; i < 8; i++) spawn(worker());
    run_scheduler();
    return 0;
}

#include <coroutine>
#include <queue>
#include <unistd.h>

// CPU-bound benchmark: 8 coroutines, each computing fib(100000) iteratively.
// Single-threaded cooperative scheduler (same as async_cpp.cpp).
// Each coroutine runs to completion without yielding — measures scheduler overhead.

struct Task {
    struct promise_type {
        Task get_return_object() noexcept {
            return Task{std::coroutine_handle<promise_type>::from_promise(*this)};
        }
        std::suspend_always initial_suspend() noexcept { return {}; }
        std::suspend_never  final_suspend()   noexcept { return {}; }
        void return_void()        noexcept {}
        void unhandled_exception() noexcept {}
    };
    std::coroutine_handle<> handle;
};

static std::queue<std::coroutine_handle<>> ready;
static void spawn(Task t) { ready.push(t.handle); }
static void run_scheduler() {
    while (!ready.empty()) {
        auto h = ready.front();
        ready.pop();
        h.resume();
    }
}

static long fib_iter(long n) {
    long a = 0, b = 1;
    for (long i = 0; i < n; ++i) {
        long tmp = a + b;
        a = b;
        b = tmp;
    }
    return b;
}

Task worker() {
    volatile long result = fib_iter(100000);
    (void)result;
    write(1, ".\n", 2);
    co_return;
}

int main() {
    for (int i = 0; i < 8; i++) spawn(worker());
    run_scheduler();
    return 0;
}

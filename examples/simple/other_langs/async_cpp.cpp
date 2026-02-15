// Minimal C++20 coroutine hello world — no external deps.
// Simulates the overhead of an async runtime: coroutine frame allocation,
// promise/awaitable machinery, and a manual scheduler pump.
#include <coroutine>
#include <cstdio>
#include <functional>
#include <queue>

struct Task {
    struct promise_type {
        Task get_return_object() { return Task{std::coroutine_handle<promise_type>::from_promise(*this)}; }
        std::suspend_never initial_suspend() { return {}; }
        std::suspend_always final_suspend() noexcept { return {}; }
        void return_void() {}
        void unhandled_exception() {}
    };
    std::coroutine_handle<promise_type> handle;
    ~Task() { if (handle && handle.done()) handle.destroy(); }
};

// Tiny scheduler: queue of ready coroutines
struct Scheduler {
    std::queue<std::coroutine_handle<>> ready;
    void spawn(std::coroutine_handle<> h) { ready.push(h); }
    void run() {
        while (!ready.empty()) {
            auto h = ready.front(); ready.pop();
            if (!h.done()) h.resume();
        }
    }
};

static Scheduler sched;

struct Yield {
    bool await_ready() { return false; }
    void await_suspend(std::coroutine_handle<> h) { sched.spawn(h); }
    void await_resume() {}
};

Task async_main() {
    co_await Yield{};
    std::printf("Hello, world!");
}

int main() {
    auto t = async_main();
    sched.spawn(t.handle);
    sched.run();
}

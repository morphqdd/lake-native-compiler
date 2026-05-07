#include <coroutine>
#include <queue>
#include <unistd.h>

// Message-passing benchmark: ping-pong, 100000 round-trips.
// Single-threaded coroutine scheduler with channel-like message passing.

static constexpr int N = 100000;
static std::queue<std::coroutine_handle<>> ready;

struct Channel {
    int value = 0;
    bool has_value = false;
    std::coroutine_handle<> waiting = nullptr;

    void send(int v) {
        value = v;
        has_value = true;
        if (waiting) {
            ready.push(waiting);
            waiting = nullptr;
        }
    }
};

struct Recv {
    Channel& ch;
    bool await_ready() noexcept { return ch.has_value; }
    void await_suspend(std::coroutine_handle<> h) noexcept { ch.waiting = h; }
    int await_resume() noexcept {
        ch.has_value = false;
        return ch.value;
    }
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

Task ponger(Channel& in, Channel& out) {
    for (int i = 0; i < N; i++) {
        int v = co_await Recv{in};
        out.send(v);
    }
}

Task pinger(Channel& in, Channel& out) {
    for (int i = 0; i < N; i++) {
        out.send(1);
        co_await Recv{in};
    }
}

int main() {
    Channel pingToPong, pongToPing;
    spawn(ponger(pingToPong, pongToPing));
    spawn(pinger(pongToPing, pingToPong));
    run_scheduler();
    write(1, ".\n", 2);
    return 0;
}

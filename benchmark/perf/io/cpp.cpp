#include <coroutine>
#include <cstdio>
#include <vector>
#include <unistd.h>

struct Task {
    struct promise_type {
        Task get_return_object() { return Task{std::coroutine_handle<promise_type>::from_promise(*this)}; }
        std::suspend_always initial_suspend() { return {}; }
        std::suspend_always final_suspend() noexcept { return {}; }
        void return_void() {}
        void unhandled_exception() {}
        std::suspend_always yield_value(int) { return {}; }
    };
    std::coroutine_handle<promise_type> h;
    bool done() { return h.done(); }
    void resume() { h.resume(); }
    ~Task() { if (h) h.destroy(); }
    Task(Task&& o) : h(o.h) { o.h = nullptr; }
    Task& operator=(Task&&) = delete;
private:
    Task(std::coroutine_handle<promise_type> h) : h(h) {}
};

Task worker(int n) {
    for (int i = 0; i < n; i++) {
        write(STDOUT_FILENO, "hello\n", 6);
        co_yield 0;
    }
}

int main() {
    std::vector<Task> tasks;
    for (int i = 0; i < 10; i++)
        tasks.push_back(worker(10000));

    bool any = true;
    while (any) {
        any = false;
        for (auto& t : tasks) {
            if (!t.done()) {
                t.resume();
                any = true;
            }
        }
    }
}

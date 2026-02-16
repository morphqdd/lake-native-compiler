#include <coroutine>
#include <queue>
#include <unistd.h>

struct Task {
  struct promise_type {
    Task get_return_object() noexcept {
      return Task{std::coroutine_handle<promise_type>::from_promise(*this)};
    }
    std::suspend_always initial_suspend() noexcept { return {}; }
    std::suspend_never final_suspend() noexcept { return {}; }
    void return_void() noexcept {}
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

Task worker(int id) {
  char msg[] = "task X\n";
  msg[5] = static_cast<char>('0' + id);
  write(1, msg, 7);
  co_return;
}

int main() {
  for (int i = 0; i < 10; i++) {
    spawn(worker(i));
  }
  run_scheduler();
  return 0;
}

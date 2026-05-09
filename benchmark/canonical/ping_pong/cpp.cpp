#include <iostream>
#include <thread>
#include <mutex>
#include <condition_variable>
#include <queue>

template <typename T> struct Chan {
    std::mutex m;
    std::condition_variable cv;
    std::queue<T> q;
    bool closed = false;

    void send(T v) {
        std::lock_guard<std::mutex> g(m);
        q.push(std::move(v));
        cv.notify_one();
    }
    bool recv(T& out) {
        std::unique_lock<std::mutex> g(m);
        cv.wait(g, [&] { return !q.empty() || closed; });
        if (q.empty()) return false;
        out = std::move(q.front());
        q.pop();
        return true;
    }
    void close() {
        std::lock_guard<std::mutex> g(m);
        closed = true;
        cv.notify_all();
    }
};

int main() {
    Chan<int> a, b;
    std::thread t([&] {
        int v;
        while (a.recv(v)) b.send(1);
    });
    for (int i = 0; i < 5; ++i) {
        a.send(1);
        int v;
        b.recv(v);
    }
    a.close();
    t.join();
    std::cout << "5 rounds completed\n";
    return 0;
}

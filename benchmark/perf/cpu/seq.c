#include <unistd.h>

// Sequential baseline: 8x fib(100000) in a simple for-loop.
// No async overhead — raw single-threaded CPU throughput.

static long fib_iter(long n) {
    long a = 0, b = 1;
    for (long i = 0; i < n; i++) {
        long tmp = a + b;
        a = b;
        b = tmp;
    }
    return b;
}

int main(void) {
    for (int i = 0; i < 8; i++) {
        volatile long r = fib_iter(100000);
        (void)r;
        write(1, ".\n", 2);
    }
    return 0;
}

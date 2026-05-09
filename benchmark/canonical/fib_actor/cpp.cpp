#include <iostream>

long fib(long n, long a, long b) {
    return n == 0 ? a : fib(n - 1, b, a + b);
}

int main() {
    std::cout << fib(20, 0, 1) << "\n";
    return 0;
}

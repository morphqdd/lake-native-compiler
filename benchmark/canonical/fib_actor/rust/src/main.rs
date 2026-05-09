fn fib(n: u64, a: u64, b: u64) -> u64 {
    if n == 0 { a } else { fib(n - 1, b, a + b) }
}

fn main() {
    println!("{}", fib(20, 0, 1));
}

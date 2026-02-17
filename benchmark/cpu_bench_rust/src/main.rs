// CPU-bound benchmark: 8 tasks, each computing fib(100000) iteratively.
// current_thread — single OS thread, matches Lake's single-threaded model.

async fn fib_iter(n: u64) -> u64 {
    let (mut a, mut b) = (0u64, 1u64);
    for _ in 0..n {
        (a, b) = (b, a.wrapping_add(b));
    }
    b
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let handles: Vec<_> = (0..8)
        .map(|_| {
            tokio::spawn(async {
                let result = fib_iter(100000).await;
                let _ = result;
                // direct write syscall — no formatting
                unsafe {
                    libc::write(1, b".\n".as_ptr() as *const _, 2);
                }
            })
        })
        .collect();

    for h in handles {
        h.await.unwrap();
    }
}

mod libc {
    extern "C" {
        pub fn write(fd: i32, buf: *const core::ffi::c_void, count: usize) -> isize;
    }
}

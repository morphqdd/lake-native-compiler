// CPU-bound benchmark: 8 tasks, fib(100000).
//
// Emulates Lake's block model: inline quantum counter per task,
// yield_now only when quantum (256) exhausted — one "block" per iteration.

use std::cell::Cell;

thread_local! {
    static QUANTUM_CTR: Cell<u32> = Cell::new(0);
}

const QUANTUM: u32 = 256;

#[inline(always)]
async fn block() {
    let q = QUANTUM_CTR.with(|c| {
        let v = c.get() + 1;
        c.set(v);
        v
    });
    if q >= QUANTUM {
        QUANTUM_CTR.with(|c| c.set(0));
        tokio::task::yield_now().await;
    }
}

async fn fib_worker() {
    let (mut a, mut b) = (0u64, 1u64);
    for _ in 0..100000u64 {
        (a, b) = (b, a.wrapping_add(b));
        block().await; // one "block" per iteration
    }
    let _ = b;
    unsafe { libc::write(1, b".\n".as_ptr() as *const _, 2); }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let handles: Vec<_> = (0..8)
        .map(|_| tokio::spawn(fib_worker()))
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

// Spawn 100k tokio tasks that exit immediately on a single-threaded runtime.

use tokio::sync::Notify;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const N: usize = 2_000;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let counter = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(Notify::new());

    for _ in 0..N {
        let c = counter.clone();
        let d = done.clone();
        tokio::spawn(async move {
            if c.fetch_add(1, Ordering::Relaxed) + 1 == N {
                d.notify_one();
            }
        });
    }
    done.notified().await;
    println!("done");
}

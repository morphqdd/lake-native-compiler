use std::io::Write;

async fn worker(n: u32) {
    let stdout = std::io::stdout();
    for _ in 0..n {
        let _ = stdout.lock().write_all(b"hello\n");
        tokio::task::yield_now().await;
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let handles: Vec<_> = (0..10)
        .map(|_| tokio::spawn(worker(10000)))
        .collect();
    for h in handles {
        h.await.unwrap();
    }
}

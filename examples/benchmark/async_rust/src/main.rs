#[tokio::main(flavor = "current_thread")]
async fn main() {
    let handles: Vec<_> = (0..10)
        .map(|i| {
            tokio::spawn(async move {
                println!("task {i}");
            })
        })
        .collect();

    for h in handles {
        h.await.unwrap();
    }
}

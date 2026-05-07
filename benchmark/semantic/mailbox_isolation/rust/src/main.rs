// mailbox-isolation on tokio mpsc.  Each task has its own channel.

use tokio::sync::mpsc;
use tokio::task;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (tx_a, mut rx_a) = mpsc::channel::<u8>(64);
    let (tx_b, mut rx_b) = mpsc::channel::<u8>(1);

    let h_a = task::spawn(async move {
        for _ in 0..50 {
            rx_a.recv().await.unwrap();
            print!("A");
        }
    });
    let h_b = task::spawn(async move {
        rx_b.recv().await.unwrap();
        print!("B");
    });

    for _ in 0..50 {
        tx_a.send(1).await.unwrap();
    }
    tx_b.send(1).await.unwrap();

    h_a.await.unwrap();
    h_b.await.unwrap();
}

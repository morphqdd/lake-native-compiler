// Message-passing benchmark: ping-pong, 100000 round-trips.
// tokio current_thread — single-threaded, matches Lake model.

use tokio::sync::mpsc;

const N: usize = 100000;

async fn ponger(mut rx: mpsc::Receiver<i64>, tx: mpsc::Sender<i64>) {
    for _ in 0..N {
        let v = rx.recv().await.unwrap();
        tx.send(v).await.unwrap();
    }
}

async fn pinger(mut rx: mpsc::Receiver<i64>, tx: mpsc::Sender<i64>) {
    for _ in 0..N {
        tx.send(1).await.unwrap();
        rx.recv().await.unwrap();
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (ping_tx, ping_rx) = mpsc::channel(1);
    let (pong_tx, pong_rx) = mpsc::channel(1);

    let h1 = tokio::spawn(ponger(ping_rx, pong_tx));
    let h2 = tokio::spawn(pinger(pong_rx, ping_tx));

    h1.await.unwrap();
    h2.await.unwrap();

    unsafe { libc::write(1, b".\n".as_ptr() as *const _, 2); }
}

mod libc {
    extern "C" {
        pub fn write(fd: i32, buf: *const core::ffi::c_void, count: usize) -> isize;
    }
}

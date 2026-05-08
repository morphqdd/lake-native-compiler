use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

static mut SINK: i64 = 0;
fn work(n: i64) {
    for i in 0..n { unsafe { SINK = SINK.wrapping_add(i) } }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let listener = TcpListener::bind("127.0.0.1:8081").await.unwrap();
    eprintln!("listening on :8081");
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            work(500);
            let _ = socket.write_all(b"hi from lake\n").await;
        });
    }
}

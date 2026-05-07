// no-starvation: tokio (current_thread) is purely cooperative — a task that
// never `.await`s starves the executor.  This binary is EXPECTED TO FAIL the
// semantic test by hanging until the test harness times it out.

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Actor A: spinner — pure CPU loop, never yields to the executor.
    tokio::spawn(async {
        let mut x: u64 = 0;
        loop {
            x = x.wrapping_add(1);
            std::hint::black_box(x);
        }
    });

    // Actor B: printer — would print "B", but never gets scheduled because
    // the executor only switches at .await points and A has none.
    tokio::spawn(async {
        println!("B");
    })
    .await
    .unwrap();
}

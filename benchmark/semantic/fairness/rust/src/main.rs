// fairness on tokio current_thread.
//
// Cooperative scheduler: each task must `.await` for others to progress.
// We yield via `task::yield_now()` inside the loop so all 4 tasks make
// progress and print "done".  Without the yield, this would still pass
// because tokio's current_thread polls multiple ready tasks per iteration —
// but the explicit yield makes the fairness intent visible.

use tokio::task;

const N: usize = 4;
const M: usize = 2_000;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        handles.push(task::spawn(async {
            let mut x = M;
            while x > 0 {
                x -= 1;
                if x % 64 == 0 {
                    task::yield_now().await;
                }
            }
            println!("done");
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

use std::sync::mpsc;
use std::thread;

fn main() {
    let (atx, arx) = mpsc::channel::<i32>();
    let (btx, brx) = mpsc::channel::<i32>();
    thread::spawn(move || {
        while arx.recv().is_ok() {
            btx.send(1).unwrap();
        }
    });
    for _ in 0..5 {
        atx.send(1).unwrap();
        brx.recv().unwrap();
    }
    drop(atx);
    println!("5 rounds completed");
}

use std::io::Read;

fn main() {
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf).unwrap();
    let lines = buf.iter().filter(|&&b| b == b'\n').count();
    println!("{}", lines);
}

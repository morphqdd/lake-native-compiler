use sha2::{Digest, Sha256};
use std::io::Write;

fn main() {
    let buf = vec![0u8; 1048576];
    let mut h = Sha256::new();
    h.update(&buf);
    let out = h.finalize();
    std::io::stdout().write_all(&out).unwrap();
}

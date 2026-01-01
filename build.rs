use std::process::Command;

use anyhow::Result;

fn main() -> Result<()> {
    assert!(
        Command::new("rustc")
            .args([
                "--crate-type=lib",
                "-C",
                "opt-level=2",
                "-C",
                "panic=abort",
                "-C",
                "relocation-model=static",
                "-C",
                "link-arg=-nostdlib",
                "--emit=obj",
                "external/syscall.rs",
                "-o",
                "external/build/syscall.a"
            ])
            .status()?
            .success()
    );
    Ok(())
}

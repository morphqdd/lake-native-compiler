//! Shared harness for integration tests.
//!
//! `run(src)` compiles the given Lake source, links it via `mold`, executes
//! it and returns exit code + captured stdout/stderr.

use std::{fs, process::Command};

use anyhow::Result;
use indicatif::ProgressBar;
use lakec::compiler::{compile, ctx::OptLevel, link};
use tempfile::tempdir;

pub struct LakeOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl LakeOutput {
    pub fn stdout_str(&self) -> &str {
        std::str::from_utf8(&self.stdout).unwrap_or("<non-utf8>")
    }
}

pub fn run(src: &str) -> Result<LakeOutput> {
    run_with_opt(src, OptLevel::None)
}

pub fn run_with_opt(src: &str, opt: OptLevel) -> Result<LakeOutput> {
    let dir = tempdir()?;
    let src_path = dir.path().join("prog.lake");
    fs::write(&src_path, src)?;
    let bytes = compile(ProgressBar::new(0), &src_path, opt)?;
    link(dir.path(), "prog", &bytes, false, "mold")?;
    let out = Command::new(dir.path().join("prog")).output()?;
    Ok(LakeOutput {
        exit_code: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

/// Convenience: assert exit 0 and stdout equals `expected`.
pub fn assert_stdout(src: &str, expected: &[u8]) {
    let out = run(src).expect("compile/run failed");
    assert_eq!(
        out.exit_code, 0,
        "non-zero exit {}: stderr={:?}",
        out.exit_code, out.stderr
    );
    assert_eq!(
        out.stdout,
        expected,
        "stdout mismatch: got {:?}, want {:?}",
        out.stdout_str(),
        std::str::from_utf8(expected).unwrap_or("<non-utf8>")
    );
}

/// Convenience: assert exit 0 and stdout contains all `needles` in order.
pub fn assert_stdout_contains_all(src: &str, needles: &[&str]) {
    let out = run(src).expect("compile/run failed");
    assert_eq!(
        out.exit_code, 0,
        "non-zero exit {}: stderr={:?}",
        out.exit_code, out.stderr
    );
    let s = out.stdout_str();
    for n in needles {
        assert!(s.contains(n), "missing {n:?} in {s:?}");
    }
}

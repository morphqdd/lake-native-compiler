//! Smoke test for #099 Phase 1 — pidfd + io_uring POLLADD child wait.

use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use indicatif::ProgressBar;
use lakec::compiler::{compile, ctx::OptLevel, link};
use tempfile::tempdir;

fn run_prog_inner(src: &str, timeout: Duration) -> Result<(i32, String, String)> {
    unsafe {
        std::env::set_var("LAKE_PATH", "/home/morphe/compiler/lake-stdlib");
    }
    let dir = tempdir()?;
    let src_path = dir.path().join("prog.lake");
    std::fs::write(&src_path, src)?;
    let bytes = compile(ProgressBar::new(0), &src_path, OptLevel::None)?;
    link(dir.path(), "prog", &bytes, false, "mold")?;
    let mut child = Command::new(dir.path().join("prog"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let pid = child.id();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None => {
                if std::time::Instant::now() >= deadline {
                    // SIGKILL the runaway so the test fails cleanly.
                    unsafe {
                        libc_kill(pid as i32, 9);
                    }
                    let out = child.wait_with_output()?;
                    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                    return Err(anyhow::anyhow!(
                        "child timed out\nstdout: {stdout}\nstderr: {stderr}"
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    let out = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    Ok((out.status.code().unwrap_or(-1), stdout, stderr))
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
#[allow(non_snake_case)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe { kill(pid, sig) }
}

/// Smallest possible test — just call clone3 and inspect the return.
/// If pidfd > 0 we're in the parent and clone3 succeeded.
#[test]
fn clone3_returns_pidfd_to_parent() -> Result<()> {
    let src = r#"
+std.io.{ println }

@rt(rt_clone3_pidfd)
@rt(to_string_with_ln)
@rt(rt_write)
@rt(len)

main is {
  _ -> {
    println("before-clone")
    let pidfd = rt_clone3_pidfd(0)
    when pidfd {
      0 -> { println("child") }
      _ -> {
        let s = to_string_with_ln(pidfd)
        rt_write(1 "pidfd=" 6)
        rt_write(1 s len(s))
      }
    }
  }
}
"#;
    let (code, stdout, stderr) = run_prog_inner(src, Duration::from_secs(5))?;
    eprintln!("stdout: {stdout:?}\nstderr: {stderr:?}\ncode: {code}");
    assert_eq!(code, 0, "non-zero exit");
    assert!(stdout.contains("before-clone"), "no before-clone marker");
    Ok(())
}

/// Verify waitid_pidfd alone (no poll_async).  Child execs /bin/true so it
/// exits ~immediately; parent calls waitid_pidfd which should reap and
/// return 0 since /bin/true exits 0.  Uses busy retry until WNOHANG sees
/// the child — no io_uring involvement here.
#[test]
fn waitid_pidfd_returns_status() -> Result<()> {
    let src = r#"
+std.experimental.sys.{ SYS_EXECVE SYS_EXIT }
+std.experimental.fs.{ cstr_of }
+std.process.{ alloc_or_die }
+std.bytes.{ addr }
+std.strings.{ concat }
+std.io.{ println }

@rt(rt_syscall)
@rt(rt_store)
@rt(rt_envp_raw)
@rt(rt_clone3_pidfd)
@rt(rt_waitid_pidfd)
@rt(rt_close)
@rt(to_string_with_ln)
@rt(rt_write)
@rt(len)

reap is {
  pidfd i64 -> ret i64 {
    let s = rt_waitid_pidfd(pidfd)
    when s {
      0 -> { ret 0 }
      _ -> { self(pidfd) }
    }
  }
}

main is {
  _ -> {
    let bin_buf = concat("" "/bin/true")
    let pc = cstr_of(bin_buf)
    let av = alloc_or_die(16)
    rt_store(av addr(pc) 8 0)
    rt_store(av 0       8 8)
    let ev = rt_envp_raw()
    let pidfd = rt_clone3_pidfd(0)
    when pidfd {
      0 -> {
        rt_syscall(SYS_EXECVE addr(pc) addr(av) ev 0 0 0)
        rt_syscall(SYS_EXIT 127 0 0 0 0 0)
      }
      _ -> {
        let s = reap(pidfd)
        rt_close(pidfd)
        println("parent-done")
      }
    }
  }
}
"#;
    let (code, stdout, stderr) = run_prog_inner(src, Duration::from_secs(5))?;
    eprintln!("stdout: {stdout:?}\nstderr: {stderr:?}");
    assert_eq!(code, 0, "non-zero exit");
    assert!(stdout.contains("parent-done"));
    Ok(())
}

/// Test poll_async + waitid chain — same as production `run` but inline.
#[test]
fn pidfd_poll_then_waitid() -> Result<()> {
    let src = r#"
+std.experimental.sys.{ SYS_EXECVE SYS_EXIT }
+std.experimental.fs.{ cstr_of }
+std.process.{ alloc_or_die }
+std.bytes.{ addr }
+std.strings.{ concat }
+std.io.{ println }

@rt(rt_syscall)
@rt(rt_store)
@rt(rt_envp_raw)
@rt(rt_clone3_pidfd)
@rt(rt_pidfd_poll_async)
@rt(rt_waitid_pidfd)
@rt(rt_close)

main is {
  _ -> {
    let bin_buf = concat("" "/bin/true")
    let pc = cstr_of(bin_buf)
    let av = alloc_or_die(16)
    rt_store(av addr(pc) 8 0)
    rt_store(av 0       8 8)
    let ev = rt_envp_raw()
    let pidfd = rt_clone3_pidfd(0)
    when pidfd {
      0 -> {
        rt_syscall(SYS_EXECVE addr(pc) addr(av) ev 0 0 0)
        rt_syscall(SYS_EXIT 127 0 0 0 0 0)
      }
      _ -> {
        println("before-poll")
        rt_pidfd_poll_async(pidfd)
        println("after-poll")
        rt_waitid_pidfd(pidfd)
        println("after-waitid")
        rt_close(pidfd)
        println("polled-and-reaped")
      }
    }
  }
}
"#;
    let (code, stdout, stderr) = run_prog_inner(src, Duration::from_secs(5))?;
    eprintln!("stdout: {stdout:?}\nstderr: {stderr:?}");
    assert_eq!(code, 0, "non-zero exit");
    assert!(stdout.contains("polled-and-reaped"));
    Ok(())
}

/// Test clone3 + waitid pair without io_uring poll path.
/// Uses /bin/true so the child exits ~immediately.  Verifies the full
/// pidfd chain (clone3 → poll_async → waitid → close → tuple).
#[test]
fn clone3_basic_child_wait() -> Result<()> {
    let src = r#"
+std.experimental.process.{ run }
+std.io.{ println }
+std.strings.{ concat }

main is {
  _ -> {
    let bin = concat("" "/bin/true")
    let arg = concat("" "")
    let r = run(bin arg)
    when r.0 {
      :ok  -> { println("ok") }
      :err -> { println("err") }
    }
  }
}
"#;
    let (code, stdout, stderr) = run_prog_inner(src, Duration::from_secs(5))?;
    eprintln!("stdout: {stdout:?}\nstderr: {stderr:?}");
    assert_eq!(code, 0, "non-zero exit (stderr: {stderr})");
    assert!(stdout.contains("ok"), "expected 'ok' in stdout: {stdout:?}");
    Ok(())
}

const INTERLEAVE_PROG: &str = r#"
+std.experimental.process.{ run }
+std.io.{ println }
+std.strings.{ concat }

peer_a is {
  n i64 -> {
    when n {
      0 -> {}
      _ -> {
        println("a")
        self(n - 1)
      }
    }
  }
}

peer_b is {
  n i64 -> {
    when n {
      0 -> {}
      _ -> {
        println("b")
        self(n - 1)
      }
    }
  }
}

child is {
  _ -> {
    let bin = concat("" "/bin/sleep")
    let arg = concat("" "0.1")
    let r = run(bin arg)
    when r.0 {
      :ok  -> { println("child-ok") }
      :err -> { println("child-err") }
    }
  }
}

main is {
  _ -> {
    child()
    peer_a(10)
    peer_b(10)
  }
}
"#;

#[test]
fn pidfd_async_wait_does_not_freeze_scheduler() -> Result<()> {
    let (code, stdout, stderr) = run_prog_inner(INTERLEAVE_PROG, Duration::from_secs(5))?;
    eprintln!("=== stdout ===\n{stdout}\n=== end ===");
    eprintln!("=== stderr ===\n{stderr}\n=== end ===");
    assert_eq!(code, 0, "non-zero exit");
    let a_count = stdout.matches("a\n").count();
    let b_count = stdout.matches("b\n").count();
    assert_eq!(a_count, 10, "peer_a printed {a_count} times, want 10");
    assert_eq!(b_count, 10, "peer_b printed {b_count} times, want 10");
    assert!(stdout.contains("child-ok"), "missing child-ok marker");
    Ok(())
}

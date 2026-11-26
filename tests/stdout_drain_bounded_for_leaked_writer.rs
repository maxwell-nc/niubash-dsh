//! The stdout pump must not be wedged by a writer the script left behind.
//!
//! `src/piped_stdout.rs` interposes a wrapper pipe between everything that
//! writes niu's stdout and the real downstream, then drains it in `finish`
//! (close our write end, wait for the pump to read EOF). That drain assumed
//! the writers end when the shell does — but a DAEMON survives its parent,
//! and Windows hands every inheritable handle to a child, so a
//! `cmd /c start /b …` grandchild keeps the wrapper write end open long
//! after the script finished. The pump then never reads EOF, `finish` never
//! returns, and niu never exits: a host that runs `niu -c '<script that
//! starts a daemon>' | cat` sees the call hang until ITS OWN timeout, and
//! every later call inherits the same wedge.
//!
//! The fix is the bounded IDLE drain in `finish`: a pump blocked in its read
//! with no bytes moving for `NIU_STDOUT_DRAIN_GRACE_MS` (default 2 s) is
//! abandoned so the process can exit; a pump blocked in a downstream WRITE
//! is a live-but-slow reader and keeps waiting, so backpressure semantics
//! are unchanged.
//!
//! The tests are deadline-bounded (the pre-fix failure mode is "niu never
//! exits") and run under a minimal environment: a host that leaks `BASH_ENV`
//! into niu would otherwise have the script's own startup work inside every
//! measurement.
//!
//! - `daemon_holding_the_wrapper_pipe_does_not_wedge_niu` — the fix.
//! - `legacy_unbounded_drain_is_still_available` — control proving the
//!   daemon really does hold the pipe (with `…GRACE_MS=0` niu must still be
//!   waiting while the daemon lives). Without it, a silently broken
//!   reproduction would make the first test pass for the wrong reason.
//! - `normal_output_is_not_truncated` — the bounded drain must not cost a
//!   normal run its bytes.

#![cfg(windows)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Ceiling for any single niu run in this file.
const EXIT_DEADLINE: Duration = Duration::from_secs(20);
/// With the 2 s default grace, a wedged-then-abandoned niu must be gone well
/// before the daemon it started dies (~10 s).
const BOUNDED_BUDGET: Duration = Duration::from_secs(6);
/// The control samples liveness here: the daemon still holds the pipe, so an
/// unbounded drain cannot have returned.
const CONTROL_SAMPLE: Duration = Duration::from_secs(4);

fn niu_binary() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_BIN_EXE_niu"));
    if p.exists() {
        return p;
    }
    let mut fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fallback.push("target");
    fallback.push("debug");
    fallback.push("niu.exe");
    fallback
}

/// A daemon that outlives the script by ~10 s. `start /b` inherits the
/// wrapper pipe write end even though its own std handles are NUL — the
/// inheritance leak this file is about.
///
/// `waitfor /t` (System32) is the delay primitive rather than `ping`: it
/// needs no network and no console, so a blocked-ICMP or console-less host
/// cannot turn the daemon into a no-op and make these tests vacuous. The
/// signal name differs per test because `waitfor` names a system-wide event.
fn daemon_script(signal: &str) -> String {
    format!("cmd /c start /b waitfor /t 10 {signal} >/dev/null 2>&1 </dev/null")
}

/// Isolated HOME/USERPROFILE so the developer's own `~/.niubashrc` cannot
/// change the timing these tests measure.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        // CARGO_TARGET_TMPDIR sits inside the workspace target directory, so
        // the scratch tree stays writable even when the host sandbox denies
        // the user temp dir (std::env::temp_dir()).
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let root = if root.as_os_str().is_empty() {
            std::env::temp_dir()
        } else {
            root
        };
        let dir = root.join(format!("stdout-drain-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        ScratchDir(dir)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spawn niu with stdout on OUR pipe (that is what installs the pump), a
/// script on `-c`, and nothing on stdin. The environment is minimal so a
/// leaked `BASH_ENV`/`ENV` from the surrounding session cannot run inside the
/// measurement — the same hermetic posture the other host pins use.
fn spawn_script(scratch: &ScratchDir, script: &str, grace_ms: Option<&str>) -> Child {
    let dir = &scratch.0;
    let mut command = Command::new(niu_binary());
    command
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "C:\\Windows\\System32;C:\\Windows")
        .env("SystemRoot", "C:\\Windows")
        .env("ComSpec", "C:\\Windows\\System32\\cmd.exe")
        .env("PATHEXT", ".COM;.EXE;.BAT;.CMD")
        .env("TEMP", dir)
        .env("TMP", dir)
        .env("HOME", dir)
        .env("USERPROFILE", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(grace) = grace_ms {
        command.env("NIU_STDOUT_DRAIN_GRACE_MS", grace);
    }
    command.spawn().expect("spawn niu")
}

/// Wait up to `budget` for the child to exit on its own. Returns the elapsed
/// time when it did, or `None` while it is still running (caller decides).
fn wait_within(child: &mut Child, budget: Duration) -> Option<Duration> {
    let started = Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => return Some(started.elapsed()),
            None => {
                if started.elapsed() >= budget {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

#[test]
fn daemon_holding_the_wrapper_pipe_does_not_wedge_niu() {
    let scratch = ScratchDir::new("daemon");
    let mut child = spawn_script(&scratch, &daemon_script("niubashdrainsignalone"), None);
    // Deliberately no stdout read: the reader stays alive and idle, so the
    // only thing that can release niu is the idle-drain bound.
    let elapsed = match wait_within(&mut child, EXIT_DEADLINE) {
        Some(elapsed) => elapsed,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "niu never exited while a leaked daemon held the wrapper pipe (> {EXIT_DEADLINE:?})"
            );
        }
    };
    let status = child.wait().expect("wait");
    assert!(
        elapsed < BOUNDED_BUDGET,
        "niu took {elapsed:?} to exit; the idle drain did not abandon the leaked writer (budget {BOUNDED_BUDGET:?})"
    );
    // The script itself succeeded; the abandoned drain must not change that.
    assert_eq!(
        status.code(),
        Some(0),
        "script status after an abandoned drain"
    );
    drop(child.stdout.take());
    drop(child.stderr.take());
}

#[test]
fn legacy_unbounded_drain_is_still_available() {
    let scratch = ScratchDir::new("control");
    let mut child = spawn_script(&scratch, &daemon_script("niubashdrainsignaltwo"), Some("0"));
    // Control: the daemon holds the pipe, so with the escape hatch set to 0
    // the drain must NOT return while the daemon lives. If this test fails
    // by returning early, the reproduction is broken (not the fix).
    let still_running = wait_within(&mut child, CONTROL_SAMPLE).is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        still_running,
        "with NIU_STDOUT_DRAIN_GRACE_MS=0 the unbounded drain returned early — the daemon is not holding the wrapper pipe, so this file proves nothing"
    );
}

#[test]
fn normal_output_is_not_truncated() {
    let scratch = ScratchDir::new("output");
    let mut child = spawn_script(&scratch, "echo drain-keeps-every-byte", None);
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut text = String::new();
    stdout.read_to_string(&mut text).expect("read niu stdout");
    let elapsed = wait_within(&mut child, EXIT_DEADLINE).expect("niu exited after EOF");
    let status = child.wait().expect("wait");
    assert!(
        text.contains("drain-keeps-every-byte"),
        "bounded drain truncated normal output: {text:?}"
    );
    assert_eq!(status.code(), Some(0), "normal run status");
    assert!(elapsed < BOUNDED_BUDGET, "normal run took {elapsed:?}");
}

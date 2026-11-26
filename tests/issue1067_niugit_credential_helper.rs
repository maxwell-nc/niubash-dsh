//! Regression tests for unixwin/niubash#1067:
//! Bundled `niu-git` must inherit host Git Credential Manager (or fall back to
//! bundled `wincred`) when the user has not explicitly configured
//! `credential.helper`, without overriding user-configured helpers or breaking
//! `GIT_TERMINAL_PROMPT=0` non-interactive fast-fail.

#![cfg(windows)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

fn unique_temp_dir(prefix: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{id}", std::process::id()))
}

fn niu_binary() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_BIN_EXE_niu"));
    if p.exists() {
        return p;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("niu.exe")
}

fn find_real_winuxcmd() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("WINUXCMD_PATH") {
        let p = PathBuf::from(raw);
        if p.is_file() {
            return Some(p);
        }
    }
    let output = Command::new("where.exe")
        .arg("winuxcmd.exe")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| PathBuf::from(line.trim()))
        .find(|path| path.is_file())
}

/// Environment variable that explicitly opts out of the winuxcmd staging
/// requirement for a local run (niubash#1104). CI never honors it: a CI
/// run without staging is a hard failure, so these pins can never again
/// pass without executing — the failure mode that let #1100's host-only
/// red escape CI.
const SKIP_STAGING_ENV: &str = "NIU_TEST_SKIP_WINUXCMD_STAGING";

fn ci_env_is_truthy() -> bool {
    std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "false")
}

/// `find_real_winuxcmd` with the niubash#1104 floor applied: missing
/// staging is a hard failure in CI and by default locally; the only skip
/// is the explicit, visible opt-out in `SKIP_STAGING_ENV`. On success the
/// staged path is printed so CI logs can prove the pin executed instead
/// of early-returning (an early return also reports `ok`).
fn require_real_winuxcmd() -> Option<PathBuf> {
    if let Some(path) = find_real_winuxcmd() {
        eprintln!("winuxcmd staging: {}", path.display());
        return Some(path);
    }
    if ci_env_is_truthy() {
        panic!(
            "winuxcmd staging is required in CI but no winuxcmd.exe was found \
             (WINUXCMD_PATH unset and `where.exe winuxcmd.exe` found nothing); \
             refusing to let this pin pass without executing (niubash#1104)"
        );
    }
    if std::env::var_os(SKIP_STAGING_ENV).is_some() {
        eprintln!(
            "SKIP: {SKIP_STAGING_ENV} is set and no winuxcmd.exe staging was found; \
             this pin did not execute (niubash#1104)"
        );
        return None;
    }
    panic!(
        "no winuxcmd.exe staging found (set WINUXCMD_PATH or put winuxcmd.exe on PATH); \
         to skip this pin explicitly on a local run, set {SKIP_STAGING_ENV}=1 (niubash#1104)"
    );
}

struct StagedShell {
    root: PathBuf,
    niu_exe: PathBuf,
    winuxcmd_usr_bin: PathBuf,
    program_files: PathBuf,
    empty_dir: PathBuf,
    gitconfig: PathBuf,
}

impl Drop for StagedShell {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stage_niu_with_niugit(tag: &str, with_wincred: bool) -> Option<StagedShell> {
    let winuxcmd_src = require_real_winuxcmd()?;
    let root = unique_temp_dir(tag);
    let winuxcmd_root = root.join("winuxcmd");
    let usr_bin = winuxcmd_root.join("usr").join("bin");
    let bin = winuxcmd_root.join("bin");
    std::fs::create_dir_all(&usr_bin).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    let niu_exe = root.join("niu.exe");
    std::fs::copy(niu_binary(), &niu_exe).unwrap();
    let winuxcmd_exe = usr_bin.join("winuxcmd.exe");
    std::fs::copy(&winuxcmd_src, &winuxcmd_exe).unwrap();
    std::fs::write(usr_bin.join("git.exe"), b"stub-niugit").unwrap();

    if with_wincred {
        let git_core = winuxcmd_root
            .join("opt")
            .join("niugit")
            .join("mingw64")
            .join("libexec")
            .join("git-core");
        std::fs::create_dir_all(&git_core).unwrap();
        std::fs::write(git_core.join("git-credential-wincred.exe"), b"stub-wincred").unwrap();
    }

    let program_files = root.join("Program Files");
    let empty_dir = root.join("empty");
    std::fs::create_dir_all(&program_files).unwrap();
    std::fs::create_dir_all(&empty_dir).unwrap();
    let gitconfig = root.join("user.gitconfig");
    std::fs::write(&gitconfig, "").unwrap();

    Some(StagedShell {
        root,
        niu_exe,
        winuxcmd_usr_bin: usr_bin,
        program_files,
        empty_dir,
        gitconfig,
    })
}

/// A hermetic `niu -c` invocation for the staged shell: the host session's
/// startup environment must not reach the pin. A developer session carries
/// `BASH_ENV` (the DSH toolchain script), `PROGRAMFILES` next to
/// `ProgramFiles`, and its own `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_*` — all of
/// which mutate exactly the variables under test. Same shape as the #1061
/// pins: `env_clear` + `--norc --noprofile` + explicit system variables.
fn base_niu_command(stage: &StagedShell, script: &str) -> Command {
    let mut cmd = Command::new(&stage.niu_exe);
    cmd.args(["--norc", "--noprofile", "-c", script]);
    cmd.env_clear();
    cmd.env(
        "SystemRoot",
        std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string()),
    );
    cmd.env("TEMP", &stage.root);
    cmd.env("TMP", &stage.root);
    cmd.env("NIU_SKIP_WINUXCMD_ACTIVATION", "1");
    cmd.env("PATH", &stage.empty_dir);
    cmd.env("ProgramFiles", &stage.program_files);
    cmd.env("ProgramFiles(x86)", &stage.empty_dir);
    cmd.env("LOCALAPPDATA", &stage.empty_dir);
    cmd.env("USERPROFILE", &stage.empty_dir);
    cmd.env("HOME", &stage.empty_dir);
    cmd.env("GIT_CONFIG_GLOBAL", &stage.gitconfig);
    // Deterministic non-tty stdin: the B1 gates (GCM_INTERACTIVE, the
    // missing-helper hint) all key off it.
    cmd.stdin(Stdio::null());
    cmd
}

fn run_stdout(cmd: &mut Command) -> String {
    let out = cmd.output().expect("spawn niu");
    assert!(
        out.status.success(),
        "niu failed: status={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn seeds_host_gcm_when_present_and_unconfigured() {
    let Some(stage) = stage_niu_with_niugit("niubash-1067-host-gcm", true) else {
        return;
    };
    let local_app_data = stage.root.join("LocalAppData");
    let gcm = local_app_data
        .join("Programs")
        .join("Git")
        .join("mingw64")
        .join("bin")
        .join("git-credential-manager.exe");
    std::fs::create_dir_all(gcm.parent().unwrap()).unwrap();
    std::fs::write(&gcm, b"stub-gcm").unwrap();

    let mut cmd = base_niu_command(
        &stage,
        r#"printf '%s|%s|%s\n' "${GIT_CONFIG_COUNT:-}" "${GIT_CONFIG_KEY_0:-}" "${GIT_CONFIG_VALUE_0:-}""#,
    );
    cmd.env("LOCALAPPDATA", &local_app_data);
    let out = run_stdout(&mut cmd);
    let expected_val = format!("!'{}'", gcm.to_string_lossy().replace('\\', "/"));
    assert_eq!(out, format!("1|credential.helper|{expected_val}"));
}

#[test]
fn falls_back_to_bundled_wincred_when_host_gcm_absent() {
    let Some(stage) = stage_niu_with_niugit("niubash-1067-wincred", true) else {
        return;
    };
    let mut cmd = base_niu_command(
        &stage,
        r#"printf '%s|%s|%s\n' "${GIT_CONFIG_COUNT:-}" "${GIT_CONFIG_KEY_0:-}" "${GIT_CONFIG_VALUE_0:-}""#,
    );
    let out = run_stdout(&mut cmd);
    assert_eq!(out, "1|credential.helper|wincred");
}

#[test]
fn does_not_override_user_unscoped_credential_helper() {
    let Some(stage) = stage_niu_with_niugit("niubash-1067-user-helper", true) else {
        return;
    };
    std::fs::write(&stage.gitconfig, "[credential]\n\thelper = store\n").unwrap();

    let mut cmd = base_niu_command(&stage, r#"printf '%s\n' "${GIT_CONFIG_COUNT:-UNSET}""#);
    let out = run_stdout(&mut cmd);
    assert_eq!(out, "UNSET");
}

#[test]
fn scoped_host_helper_does_not_suppress_fallback_helper() {
    let Some(stage) = stage_niu_with_niugit("niubash-1067-scoped-helper", true) else {
        return;
    };
    std::fs::write(
        &stage.gitconfig,
        "[credential \"https://github.com\"]\n\thelper = !gh auth git-credential\n",
    )
    .unwrap();

    let mut cmd = base_niu_command(
        &stage,
        r#"printf '%s|%s|%s\n' "${GIT_CONFIG_COUNT:-}" "${GIT_CONFIG_KEY_0:-}" "${GIT_CONFIG_VALUE_0:-}""#,
    );
    let out = run_stdout(&mut cmd);
    assert_eq!(out, "1|credential.helper|wincred");
}

/// B1 gap (a): a foreign git installation ahead of the bundled dirs that
/// carries a helper of its own in its system gitconfig keeps it — niu does
/// not plant a fallback over it.
#[test]
fn does_not_seed_when_foreign_first_git_configures_its_own_helper() {
    let Some(stage) = stage_niu_with_niugit("niubash-1067-user-git", true) else {
        return;
    };
    let foreign_cmd = stage_foreign_git(&stage, "Git", Some("[credential]\n\thelper = manager\n"));

    let mut cmd = base_niu_command(&stage, r#"printf '%s\n' "${GIT_CONFIG_COUNT:-UNSET}""#);
    cmd.env("PATH", &foreign_cmd);
    let out = run_stdout(&mut cmd);
    assert_eq!(out, "UNSET");
}

/// B1 gap (a): the common Git-for-Windows-ahead-of-the-bundled-dirs machine
/// — a foreign git whose installation configures no helper of its own — now
/// gets the fallback helper planted, instead of the user having to pass
/// `-c credential.helper=manager` by hand.
#[test]
fn seeds_for_foreign_first_git_without_its_own_helper() {
    let Some(stage) = stage_niu_with_niugit("niubash-b1-foreign-git", true) else {
        return;
    };
    let foreign_cmd = stage_foreign_git(&stage, "Git", None);

    let mut cmd = base_niu_command(
        &stage,
        r#"printf '%s|%s|%s\n' "${GIT_CONFIG_COUNT:-}" "${GIT_CONFIG_KEY_0:-}" "${GIT_CONFIG_VALUE_0:-}""#,
    );
    cmd.env("PATH", &foreign_cmd);
    let output = cmd.output().expect("spawn niu");
    assert!(
        output.status.success(),
        "niu failed: srderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(out, "1|credential.helper|wincred");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("no git credential helper"),
        "a resolved helper must not produce the missing-helper hint; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// B1 gap (b): a host GCM found on `PATH` is planted under the pinned helper
/// name `manager` — the same spelling the workaround
/// `git -c credential.helper=manager` uses.
#[test]
fn prefers_pinned_manager_helper_when_gcm_is_on_path() {
    let Some(stage) = stage_niu_with_niugit("niubash-b1-manager-name", true) else {
        return;
    };
    let foreign_cmd = stage_foreign_git(&stage, "Git", None);
    std::fs::write(foreign_cmd.join("git-credential-manager.exe"), b"stub-gcm").unwrap();

    let mut cmd = base_niu_command(
        &stage,
        r#"printf '%s|%s|%s\n' "${GIT_CONFIG_COUNT:-}" "${GIT_CONFIG_KEY_0:-}" "${GIT_CONFIG_VALUE_0:-}""#,
    );
    cmd.env("PATH", &foreign_cmd);
    let out = run_stdout(&mut cmd);
    assert_eq!(out, "1|credential.helper|manager");
}

/// B1 gap (b): non-tty sessions get `GCM_INTERACTIVE=never` so a planted (or
/// user-configured) GCM cannot try to prompt; an explicit user value wins.
#[test]
fn seeds_gcm_interactive_never_for_non_tty_and_keeps_user_value() {
    let Some(stage) = stage_niu_with_niugit("niubash-b1-gcm-interactive", true) else {
        return;
    };

    let mut cmd = base_niu_command(&stage, r#"printf '%s\n' "${GCM_INTERACTIVE:-unset}""#);
    let out = run_stdout(&mut cmd);
    assert_eq!(out, "never");

    let mut user_cmd = base_niu_command(&stage, r#"printf '%s\n' "${GCM_INTERACTIVE:-unset}""#);
    user_cmd.env("GCM_INTERACTIVE", "auto");
    let out = run_stdout(&mut user_cmd);
    assert_eq!(
        out, "auto",
        "an explicit user value must never be overridden"
    );
}

/// B1 gap (c): with no helper resolvable at all, a non-tty session that runs
/// `git` gets exactly one short stderr hint naming both workarounds.
#[test]
fn hint_fires_once_when_no_helper_resolves_noninteractively() {
    let Some(stage) = stage_niu_with_niugit("niubash-b1-hint", false) else {
        return;
    };
    let mut cmd = base_niu_command(&stage, "git --version");
    let output = cmd.output().expect("spawn niu");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no git credential helper"),
        "expected the missing-helper hint, stderr={stderr}"
    );
    assert!(
        stderr.contains("git -c credential.helper=manager"),
        "hint must name the -c workaround, stderr={stderr}"
    );
    assert!(
        stderr.contains("NIU_USER_PATH_FIRST=1"),
        "hint must name the PATH switch, stderr={stderr}"
    );
    assert_eq!(
        stderr.matches("no git credential helper").count(),
        1,
        "the hint must appear at most once per process, stderr={stderr}"
    );
}

/// B1 gap (c): the hint stays out of sessions that never run `git`, and out
/// of tty sessions (which can still be prompted).
#[test]
fn hint_is_absent_for_non_git_command_and_in_tty_session() {
    let Some(stage) = stage_niu_with_niugit("niubash-b1-hint-gates", false) else {
        return;
    };

    let mut quiet = base_niu_command(&stage, "echo hello");
    let output = quiet.output().expect("spawn niu");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("no git credential helper"),
        "a session that never runs git must stay quiet, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Same stage under a pseudo-terminal: stdin is a tty, so no hint.
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    let mut builder = CommandBuilder::new(&stage.niu_exe);
    builder.env_clear();
    if let Ok(system_root) = std::env::var("SystemRoot") {
        builder.env("SystemRoot", system_root);
    }
    builder.env("TEMP", stage.root.display().to_string());
    builder.env("TMP", stage.root.display().to_string());
    builder.env("NIU_SKIP_WINUXCMD_ACTIVATION", "1");
    builder.env("PATH", stage.empty_dir.display().to_string());
    builder.env("ProgramFiles", stage.program_files.display().to_string());
    builder.env("ProgramFiles(x86)", stage.empty_dir.display().to_string());
    builder.env("LOCALAPPDATA", stage.empty_dir.display().to_string());
    builder.env("USERPROFILE", stage.empty_dir.display().to_string());
    builder.env("HOME", stage.empty_dir.display().to_string());
    builder.env("GIT_CONFIG_GLOBAL", stage.gitconfig.display().to_string());
    builder.env("NIU_LANG", "en");
    builder.env("TERM", "xterm");
    builder.args(["--norc", "--noprofile", "-c", "git --version"]);

    let mut child = pair.slave.spawn_command(builder).expect("spawn niu in pty");
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("take pty writer");
    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let collected_bg = std::sync::Arc::clone(&collected);
    let _pump = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            let chunk = &buf[..n];
            // The #1061 pty pin's rule: a startup cursor-position query must
            // be answered or the child waits on the terminal forever.
            if chunk.windows(4).any(|w| w == b"\x1b[6n") {
                let _ = writer.write_all(b"\x1b[1;1R");
                let _ = writer.flush();
            }
            collected_bg.lock().unwrap().extend_from_slice(chunk);
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if child.try_wait().expect("poll pty child").is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("pty child hung");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Mirror the #1061 pty pin: give the pump a moment, close the master end
    // so the reader sees EOF, and never join it (a ConPTY reader can outlive
    // the child; the test process ends right after).
    std::thread::sleep(std::time::Duration::from_millis(100));
    drop(pair.master);
    let output = String::from_utf8_lossy(&collected.lock().unwrap()).into_owned();
    assert!(
        !output.contains("no git credential helper"),
        "a tty session must not be hinted, output={output:?}"
    );
}

/// A Git-for-Windows-shaped foreign installation: `<dir>\cmd\git.exe`, with
/// an optional system gitconfig at `<dir>\etc\gitconfig`. Returns the `cmd`
/// directory to put on `PATH`.
fn stage_foreign_git(stage: &StagedShell, name: &str, system_config: Option<&str>) -> PathBuf {
    let root = stage.program_files.join(name);
    let cmd_dir = root.join("cmd");
    std::fs::create_dir_all(&cmd_dir).unwrap();
    std::fs::write(cmd_dir.join("git.exe"), b"user-git").unwrap();
    if let Some(config) = system_config {
        let etc = root.join("etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("gitconfig"), config).unwrap();
    }
    cmd_dir
}

#[test]
fn appends_to_existing_git_config_count_and_respects_git_askpass() {
    let Some(stage) = stage_niu_with_niugit("niubash-1067-append-askpass", true) else {
        return;
    };

    // Append to existing GIT_CONFIG_COUNT=1.
    let mut append_cmd = base_niu_command(
        &stage,
        r#"printf '%s|%s|%s|%s|%s\n' "${GIT_CONFIG_COUNT:-}" "${GIT_CONFIG_KEY_0:-}" "${GIT_CONFIG_VALUE_0:-}" "${GIT_CONFIG_KEY_1:-}" "${GIT_CONFIG_VALUE_1:-}""#,
    );
    append_cmd.env("GIT_CONFIG_COUNT", "1");
    append_cmd.env("GIT_CONFIG_KEY_0", "user.name");
    append_cmd.env("GIT_CONFIG_VALUE_0", "Alice");
    let out = run_stdout(&mut append_cmd);
    assert_eq!(out, "2|user.name|Alice|credential.helper|wincred");

    // Respect GIT_ASKPASS when set.
    let mut askpass_cmd = base_niu_command(&stage, r#"printf '%s\n' "${GIT_CONFIG_COUNT:-UNSET}""#);
    askpass_cmd.env("GIT_ASKPASS", "custom-askpass.exe");
    let out_askpass = run_stdout(&mut askpass_cmd);
    assert_eq!(out_askpass, "UNSET");

    // Ensure winuxcmd_usr_bin is not unused warning.
    let _ = Path::new(&stage.winuxcmd_usr_bin);
}

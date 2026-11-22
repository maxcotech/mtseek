//! Service mode end to end (ADR 0014), as an elevated administrator:
//!
//! * `mtseekd --install` registers a test service under its own name, pipe and cache, and
//!   the SCM starts it as `LocalSystem`.
//! * The cache folder someone else created first is made safe: owned by SYSTEM or
//!   Administrators, with no entry for Everyone.
//! * A second local account searches through the service and gets exactly what its own
//!   `rg` prints: the shared file, and "Access is denied" for the administrator's private
//!   file, whose bytes the service holds (invariant 5, the two-account test of ADR 0004).
//!
//! Skipped unless elevated, and unless `rg` is on PATH; `MTSEEK_REQUIRE_ADMIN=1` (CI) makes
//! a skip a failure.

#![cfg(windows)]
#![allow(clippy::print_stderr)] // why the test was skipped, and which step it is on

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, mpsc};
use std::time::{Duration, Instant};

use mtseek_ntfs::security::{self, Token};

/// Notes the step the test is on. The harness shows these if the test fails, so a step
/// that runs out of time (on CI, in a session without a desktop) says where.
fn progress(step: &str) {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    eprintln!("[{:>6.1}s] {step}", start.elapsed().as_secs_f64());
}

/// What a child printed on one pipe so far, and a signal that the pipe ended.
type Pipe = (Arc<Mutex<Vec<u8>>>, mpsc::Receiver<()>);

/// Runs `command` to the end, or gives up after `limit` and says what it printed so far.
fn try_bounded(
    command: &mut Command,
    limit: Duration,
    step: &str,
) -> Result<std::process::Output, String> {
    progress(step);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("{step}: {err}"))?;
    // Read both pipes on threads, so a full pipe never stalls the child. A process it
    // started may inherit them and hold them open after it ends, so what was read so far
    // is kept as it arrives, and the end is waited for only briefly.
    let drain = |pipe: Option<Box<dyn Read + Send>>| -> Pipe {
        let read = Arc::new(Mutex::new(Vec::new()));
        let (done, ended) = mpsc::channel();
        let into = Arc::clone(&read);
        std::thread::spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut chunk = [0u8; 4096];
                while let Ok(n @ 1..) = pipe.read(&mut chunk) {
                    into.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .extend_from_slice(&chunk[..n]);
                }
            }
            let _ = done.send(());
        });
        (read, ended)
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let collect = |(read, ended): &Pipe| {
        let _ = ended.recv_timeout(Duration::from_secs(2));
        std::mem::take(&mut *read.lock().unwrap_or_else(PoisonError::into_inner))
    };
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(std::process::Output {
                    status,
                    stdout: collect(&stdout),
                    stderr: collect(&stderr),
                });
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{step}: still running after {limit:?}\nstdout: {}\nstderr: {}",
                    String::from_utf8_lossy(&collect(&stdout)),
                    String::from_utf8_lossy(&collect(&stderr)),
                ));
            }
            Err(err) => return Err(format!("{step}: {err}")),
        }
    }
}

/// [`try_bounded`], failing the test if the step can't run or runs out of time.
fn bounded(command: &mut Command, limit: Duration, step: &str) -> std::process::Output {
    try_bounded(command, limit, step).unwrap_or_else(|err| panic!("{err}"))
}

fn elevated() -> bool {
    let admin = Token::current().is_ok_and(|t| t.is_admin());
    let rg = Command::new("rg")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !(admin && rg) {
        assert!(
            std::env::var_os("MTSEEK_REQUIRE_ADMIN").is_none(),
            "MTSEEK_REQUIRE_ADMIN is set, but elevated: {admin}, rg on PATH: {rg}"
        );
        eprintln!("skipping the service test: needs an elevated terminal and rg");
    }
    admin && rg
}

/// Undoes what the test set up, whatever happens.
#[derive(Default)]
struct Cleanup {
    service: Option<String>,
    user: Option<String>,
    dirs: Vec<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        // Nothing here may panic: this runs while a failed test unwinds.
        if let Some(name) = &self.service {
            progress("cleanup: uninstalling the test service");
            let _ = mtseek_ntfs::service::uninstall(name, Duration::from_secs(30));
        }
        if let Some(user) = &self.user {
            let limit = Duration::from_secs(60);
            let _ = try_bounded(
                Command::new("net").args(["user", user, "/delete"]),
                limit,
                "cleanup: deleting the test account",
            );
            let _ = try_bounded(
                &mut powershell(&format!(
                    "Get-CimInstance Win32_UserProfile | Where-Object {{ $_.LocalPath -like '*\\{user}' }} | Remove-CimInstance"
                )),
                limit,
                "cleanup: deleting its profile",
            );
        }
        for dir in &self.dirs {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

/// A PowerShell command running `script`.
fn powershell(script: &str) -> Command {
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    command
}

/// Exit code, stdout and stderr lines (without the program's name), sorted.
type Output = (String, BTreeSet<String>, BTreeSet<String>);

/// Runs `command` (a cmd.exe line) as `user`, collecting what it prints in `out`.
fn run_as(user: &str, password: &str, command: &str, out: &Path, tag: &str) -> Output {
    let (stdout, stderr, code) = (
        out.join(format!("{tag}.out")),
        out.join(format!("{tag}.err")),
        out.join(format!("{tag}.code")),
    );
    let line = format!(
        // `!ERRORLEVEL!` (delayed expansion, `/v:on`): `%ERRORLEVEL%` would be read when the
        // line is parsed, before the command runs.
        "{command} > \"{}\" 2> \"{}\" & echo !ERRORLEVEL! > \"{}\"",
        stdout.display(),
        stderr.display(),
        code.display()
    );
    // `/s` and the outer quotes: cmd strips exactly those. Without `/s`, a line that starts
    // with a quote (a quoted program path) loses its first and last quote and doesn't run.
    // cmd.exe's own exit code is printed: if it never ran the line, it says why (for
    // example 0xC0000142, a process that couldn't start in this session).
    let script = format!(
        "$p = ConvertTo-SecureString '{password}' -AsPlainText -Force; \
         $c = New-Object System.Management.Automation.PSCredential('{user}', $p); \
         $r = Start-Process -FilePath cmd.exe -ArgumentList '/v:on', '/s', '/c', '\"{}\"' -Credential $c \
         -LoadUserProfile -WorkingDirectory '{}' -Wait -PassThru -WindowStyle Hidden; \
         'cmd.exe exited with 0x{{0:X8}}' -f $r.ExitCode",
        line.replace('\'', "''"),
        out.display()
    );
    let ran = bounded(
        &mut powershell(&script),
        Duration::from_secs(120),
        &format!("running {tag} as {user}"),
    );
    let said = format!(
        "powershell: {}{}",
        String::from_utf8_lossy(&ran.stdout).trim(),
        String::from_utf8_lossy(&ran.stderr).trim()
    );
    progress(&said);
    assert!(
        ran.status.success(),
        "running {tag} as {user} failed: {said}"
    );
    assert!(code.exists(), "{tag} didn't run as {user}: {said}");
    let lines = |path: &Path| -> BTreeSet<String> {
        String::from_utf8_lossy(&fs::read(path).unwrap_or_default())
            .lines()
            .map(|l| {
                l.strip_prefix("rg: ")
                    .or_else(|| l.strip_prefix("mtseek: "))
                    .unwrap_or(l)
                    .to_owned()
            })
            .collect()
    };
    let code = fs::read_to_string(&code)
        .unwrap_or_default()
        .trim()
        .to_owned();
    (code, lines(&stdout), lines(&stderr))
}

/// `rg.exe` and `mtseek.exe` copied where the second account may run them (`bin`): `rg`
/// is often installed in the administrator's own profile. Returns their paths.
///
/// A copy of `rg` that doesn't run is a launcher that finds the real program relative to
/// itself (Chocolatey's, on CI); those live where every user may run them, so the original
/// is used.
#[allow(clippy::unwrap_used)] // test setup: a failure is the test failing
fn programs(bin: &Path, mtseek: &Path) -> (PathBuf, PathBuf) {
    let limit = Duration::from_secs(30);
    let found = bounded(Command::new("where.exe").arg("rg"), limit, "finding rg");
    let rg = String::from_utf8_lossy(&found.stdout)
        .lines()
        .next()
        .map(|l| PathBuf::from(l.trim()))
        .expect("rg on PATH");
    let copies = (bin.join("rg.exe"), bin.join("mtseek.exe"));
    fs::copy(&rg, &copies.0).unwrap();
    fs::copy(mtseek, &copies.1).unwrap();
    let runs = try_bounded(
        Command::new(&copies.0).arg("--version"),
        limit,
        "checking that a copy of rg runs",
    )
    .is_ok_and(|o| o.status.success());
    progress(&format!(
        "rg: {}",
        if runs { "the copy" } else { "in place" }
    ));
    if runs { copies } else { (rg, copies.1) }
}

/// The tree (everyone may read it, but one private file), the output folder (everyone may
/// write it), the programs folder (everyone may run what's in it) and a cache folder
/// someone else made first, open to Everyone.
#[allow(clippy::unwrap_used)] // test setup: a failure is the test failing
fn prepare(base: &Path, tree: &Path, out: &Path, cache: &Path) {
    for dir in [tree, out, cache, &base.join("bin")] {
        fs::create_dir_all(dir).unwrap();
    }
    let me = mtseek_ntfs::pipe::user_sid().unwrap();
    security::set_dacl(
        base,
        &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)(A;OICI;0x1200a9;;;BU)"),
    )
    .unwrap();
    security::set_dacl(out, &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;BU)")).unwrap();
    fs::write(
        tree.join("shared.txt"),
        "a shared needle
",
    )
    .unwrap();
    let private = tree.join("private.txt");
    fs::write(
        &private,
        "a private needle
",
    )
    .unwrap();
    security::set_dacl(&private, &format!("D:P(A;;FA;;;{me})(A;;FA;;;SY)")).unwrap();
    security::set_dacl(cache, "D:P(A;OICI;FA;;;WD)").unwrap();
}

/// `mtseekd --install` a test service, and waits until it serves on `pipe`.
fn install(daemon: &Path, name: &str, pipe: &str, cache: &Path) {
    let installed = bounded(
        Command::new(daemon)
            .args(["--install", "--name", name, "--pipe", pipe, "--cache"])
            .arg(cache),
        Duration::from_secs(60),
        "installing the test service",
    );
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    progress("waiting for the service to answer");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // On a thread: a server that accepts but never answers `initialize` would block.
        let (send, receive) = mpsc::channel();
        let name = pipe.to_owned();
        std::thread::spawn(move || {
            let client = mtseek_ipc::client::Client::connect(
                &name,
                Duration::from_millis(200),
                mtseek_ntfs::pipe::LOCAL_SYSTEM,
            );
            let _ = send.send(client.map(|c| c.is_some()).map_err(|e| e.to_string()));
        });
        let left = deadline.saturating_duration_since(Instant::now());
        match receive.recv_timeout(left) {
            Ok(Ok(true)) => return,
            Ok(answer) => {
                assert!(
                    Instant::now() < deadline,
                    "the service never served: {answer:?}"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(err) => panic!("the service accepted a connection but never answered: {err}"),
        }
    }
}

/// A second local account. Returns its name and password.
fn add_user(pid: u32) -> (String, String) {
    let user = format!("mtseek-t{}", pid % 100_000);
    let password = format!("Mt!{pid:x}-Aa{}", pid % 97);
    let added = bounded(
        Command::new("net").args(["user", &user, &password, "/add"]),
        Duration::from_secs(60),
        "adding a second account",
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stdout)
    );
    (user, password)
}

/// The helper that keeps a hung step from hanging CI, which runs as any user.
#[test]
fn a_step_that_runs_too_long_fails_and_says_which() {
    let done = bounded(
        Command::new("cmd").args(["/c", "echo out& echo err 1>&2"]),
        Duration::from_secs(30),
        "echo",
    );
    assert!(done.status.success());
    assert_eq!(String::from_utf8_lossy(&done.stdout).trim(), "out");
    assert_eq!(String::from_utf8_lossy(&done.stderr).trim(), "err");
    let started = Instant::now();
    let err = try_bounded(
        Command::new("cmd").args(["/c", "echo early& ping -n 30 127.0.0.1 > nul"]),
        Duration::from_millis(500),
        "a slow step",
    )
    .expect_err("it runs out of time");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert!(err.starts_with("a slow step: still running after"), "{err}");
    assert!(err.contains("early"), "{err}");
}

#[test]
fn a_second_account_sees_through_the_service_what_its_own_rg_sees() {
    if !elevated() {
        return;
    }
    let pid = std::process::id();
    let mut cleanup = Cleanup::default();
    let daemon = PathBuf::from(env!("CARGO_BIN_EXE_mtseekd"));
    let mtseek = daemon.with_file_name("mtseek.exe");
    assert!(
        mtseek.exists(),
        "build the CLI first (cargo build -p mtseek-cli)"
    );
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let base = PathBuf::from(format!(r"{drive}\mtseek-test-{pid}"));
    let (tree, out, cache) = (base.join("tree"), base.join("out"), base.join("cache"));
    cleanup.dirs.push(base.clone());
    prepare(&base, &tree, &out, &cache);
    let (rg, mtseek_copy) = programs(&base.join("bin"), &mtseek);
    let name = format!("mtseek-test-{pid}");
    let pipe = format!(r"\\.\pipe\mtseek-test-service-{pid}");
    // Before installing: if waiting for it fails, the service must still be removed.
    cleanup.service = Some(name.clone());
    install(&daemon, &name, &pipe, &cache);

    // The cache folder was made safe.
    let owner_ok = {
        let sd = security::descriptor_at(&cache).unwrap();
        let everyone = [1u8, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        !sd.windows(everyone.len()).any(|w| w == everyone)
    };
    assert!(owner_ok, "the cache folder still grants Everyone");

    // Index the tree through the service (as this administrator). `-u`: the global excludes
    // file of whoever runs the test must not matter.
    let env = |c: &mut Command| {
        c.env("MTSEEK_PIPE", r"\\.\pipe\mtseek-test-nobody")
            .env("MTSEEK_SERVICE_PIPE", &pipe);
    };
    let mut index = Command::new(&mtseek);
    env(&mut index);
    let indexed = bounded(
        index
            .args(["--daemon", "--index", "--stats", "-u", "needle"])
            .arg(&tree),
        Duration::from_secs(120),
        "indexing the tree through the service",
    );
    let stats = String::from_utf8_lossy(&indexed.stderr);
    assert!(stats.contains("answered by mtseekd"), "{stats}");

    let (user, password) = add_user(pid);
    cleanup.user = Some(user.clone());

    let args = format!(
        "-u --line-number --no-heading --with-filename needle \"{}\"",
        tree.display()
    );
    let expected = run_as(
        &user,
        &password,
        &format!("\"{}\" --no-config {args}", rg.display()),
        &out,
        "rg",
    );
    let actual = run_as(
        &user,
        &password,
        &format!(
            "set MTSEEK_PIPE=\\\\.\\pipe\\mtseek-test-nobody&& set MTSEEK_SERVICE_PIPE={pipe}&& \"{}\" --daemon --stats {args}",
            mtseek_copy.display()
        ),
        &out,
        "mtseek",
    );
    assert!(
        actual.2.iter().any(|l| l.contains("answered by mtseekd")),
        "answered by the service: {:?}",
        actual.2
    );
    let errors: BTreeSet<String> = actual
        .2
        .iter()
        .filter(|l| l.contains("os error"))
        .cloned()
        .collect();
    assert_eq!(
        expected.0, "2",
        "rg can't open the private file: {expected:?}"
    );
    assert_eq!(
        (&actual.0, &actual.1, &errors),
        (&expected.0, &expected.1, &expected.2)
    );
    assert!(
        actual.1.iter().all(|l| !l.contains("private")),
        "{:?}",
        actual.1
    );
}

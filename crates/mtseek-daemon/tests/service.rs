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
#![allow(clippy::print_stderr)] // tells the developer why the test was skipped

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use mtseek_ntfs::security::{self, Token};

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
        if let Some(name) = &self.service {
            let _ = mtseek_ntfs::service::uninstall(name, Duration::from_secs(30));
        }
        if let Some(user) = &self.user {
            let _ = Command::new("net").args(["user", user, "/delete"]).output();
            let _ = powershell(&format!(
                "Get-CimInstance Win32_UserProfile | Where-Object {{ $_.LocalPath -like '*\\{user}' }} | Remove-CimInstance"
            ));
        }
        for dir in &self.dirs {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

fn powershell(script: &str) -> std::process::Output {
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .expect("powershell runs")
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
    let script = format!(
        "$p = ConvertTo-SecureString '{password}' -AsPlainText -Force; \
         $c = New-Object System.Management.Automation.PSCredential('{user}', $p); \
         Start-Process -FilePath cmd.exe -ArgumentList '/v:on', '/c', '{}' -Credential $c \
         -LoadUserProfile -WorkingDirectory '{}' -Wait -WindowStyle Hidden",
        line.replace('\'', "''"),
        out.display()
    );
    let ran = powershell(&script);
    assert!(
        ran.status.success(),
        "running as {user} failed: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
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
#[allow(clippy::unwrap_used)] // test setup: a failure is the test failing
fn programs(bin: &Path, mtseek: &Path) -> (PathBuf, PathBuf) {
    let found = Command::new("where.exe").arg("rg").output().unwrap();
    let rg = String::from_utf8_lossy(&found.stdout)
        .lines()
        .next()
        .map(|l| PathBuf::from(l.trim()))
        .expect("rg on PATH");
    let copies = (bin.join("rg.exe"), bin.join("mtseek.exe"));
    fs::copy(&rg, &copies.0).unwrap();
    fs::copy(mtseek, &copies.1).unwrap();
    copies
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
#[allow(clippy::unwrap_used)] // test setup: a failure is the test failing
fn install(daemon: &Path, name: &str, pipe: &str, cache: &Path) {
    let installed = Command::new(daemon)
        .args(["--install", "--name", name, "--pipe", pipe, "--cache"])
        .arg(cache)
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while mtseek_ipc::client::Client::connect(
        pipe,
        Duration::from_millis(200),
        mtseek_ntfs::pipe::LOCAL_SYSTEM,
    )
    .ok()
    .flatten()
    .is_none()
    {
        assert!(Instant::now() < deadline, "the service never served");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A second local account. Returns its name and password.
#[allow(clippy::unwrap_used)] // test setup: a failure is the test failing
fn add_user(pid: u32) -> (String, String) {
    let user = format!("mtseek-t{}", pid % 100_000);
    let password = format!("Mt!{pid:x}-Aa{}", pid % 97);
    let added = Command::new("net")
        .args(["user", &user, &password, "/add"])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stdout)
    );
    (user, password)
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
    install(&daemon, &name, &pipe, &cache);
    cleanup.service = Some(name);

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
    let indexed = index
        .args(["--daemon", "--index", "--stats", "-u", "needle"])
        .arg(&tree)
        .output()
        .unwrap();
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

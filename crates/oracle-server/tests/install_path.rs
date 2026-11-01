//! The first minute of a stranger's install, through the `oracle` binary itself: a server is never started on a home
//! that `oracle init` did not make, and the home `oracle init` makes is this user's alone.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn oracle() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oracle"))
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("oracle-install-{tag}-{}-{}", std::process::id(), oracle_proto::random_hex(4)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn init(home: &Path) {
    let out = oracle().arg("--home").arg(home).args(["init", "--owner", "stranger"]).env_remove("ORACLE_HOME").output().unwrap();
    assert!(out.status.success(), "init: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn serve_refuses_a_home_that_was_never_initialised_and_creates_nothing() {
    let root = scratch("never");
    let home = root.join("server");
    let mut srv = oracle().arg("--home").arg(&home).args(["serve", "--bind", "127.0.0.1:0"]).env_remove("ORACLE_HOME")
        .stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    // Bounded: a server that does start never exits on its own, so it is stopped and the test fails, not hangs.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(s) = srv.try_wait().unwrap() {
            break Some(s);
        }
        if std::time::Instant::now() > deadline {
            srv.kill().ok();
            srv.wait().ok();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let mut err = String::new();
    std::io::Read::read_to_string(&mut srv.stderr.take().unwrap(), &mut err).unwrap();
    let created = home.exists();
    std::fs::remove_dir_all(&root).ok();
    assert!(status.is_some_and(|s| !s.success()), "a server on a home init never made must refuse, not serve: {err}");
    assert!(err.contains("no Oracle in") && err.contains("oracle init --owner"), "{err}");
    assert!(!created, "nothing was created: {}", home.display());
}

#[test]
fn serve_starts_on_a_home_that_init_made() {
    // The control: the same command on an initialised home listens.
    let root = scratch("made");
    let home = root.join("server");
    init(&home);
    let mut srv = oracle().arg("--home").arg(&home).args(["serve", "--bind", "127.0.0.1:0"]).env_remove("ORACLE_HOME")
        .stderr(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(srv.stderr.take().unwrap()).read_line(&mut line).unwrap();
    srv.kill().ok();
    srv.wait().ok();
    assert!(line.contains("listening on http://"), "{line}");
    std::fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn a_home_init_creates_is_private_whatever_the_umask() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("private");
    // Group-writable parent, as a umask of 002 leaves a user's directories on Ubuntu.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o775)).unwrap();
    let home = root.join("server");
    init(&home);
    let mode = std::fs::metadata(&home).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "the new home is {mode:o}");
    assert!(home.join("oracle.db").exists() && home.join("keys/stranger.key").exists());
    // An existing directory is the caller's choice and is left as it is.
    let given = root.join("given");
    std::fs::create_dir_all(&given).unwrap();
    std::fs::set_permissions(&given, std::fs::Permissions::from_mode(0o750)).unwrap();
    init(&given);
    assert_eq!(std::fs::metadata(&given).unwrap().permissions().mode() & 0o777, 0o750);
    std::fs::remove_dir_all(&root).ok();
}

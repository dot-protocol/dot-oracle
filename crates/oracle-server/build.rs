use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// The commit this binary is built from, stamped as ORACLE_BUILD_COMMIT; `+dirty` when tracked files differ from it.
/// Rebuilt when the branch moves, not only when HEAD names another branch: `.git/HEAD` reads "ref: refs/heads/dev"
/// before and after a commit, so watching it alone left the stamp at an old commit (Emmy, run 3's receipt said 3371e63
/// for a 60af43c binary).
fn main() {
    let sha = std::env::var("ORACLE_BUILD_COMMIT").ok().or_else(|| {
        let head = git(&["rev-parse", "HEAD"])?;
        let dirty = !Command::new("git")
            .args(["diff", "--quiet", "HEAD", "--"])
            .status()
            .map(|s| s.success())
            .unwrap_or(true);
        Some(if dirty { format!("{head}+dirty") } else { head })
    });
    println!("cargo:rustc-env=ORACLE_BUILD_COMMIT={}", sha.unwrap_or_else(|| "unknown".into()));
    println!("cargo:rerun-if-env-changed=ORACLE_BUILD_COMMIT");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
    println!("cargo:rerun-if-changed=../../.git/index");
    if let Ok(head) = std::fs::read_to_string("../../.git/HEAD") {
        if let Some(r) = head.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed=../../.git/{r}");
        }
    }
}

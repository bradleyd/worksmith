//! Stamp the binary with its commit. `--version` saying only "0.2.1" is how a
//! stale binary went unnoticed twice in one week: main and a feature branch
//! print the same thing, and a shared target-dir makes the mixup easy.

fn main() {
    let hash = std::process::Command::new("git")
        .args(["describe", "--always", "--dirty", "--abbrev=9"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=WS_GIT_HASH={hash}");
    // Re-stamp when HEAD moves (commit, checkout), not on every build.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
}

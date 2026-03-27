use std::process::Command;

fn main() {
    // Get the short git hash for version string
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GRU_GIT_HASH={git_hash}");

    // Rebuild only when the current commit changes.
    // Watch HEAD and the specific ref it points to (e.g. refs/heads/main),
    // NOT the entire refs/ directory which gets touched by `git pull` updating
    // remote tracking refs — causing unnecessary full rebuilds.
    let git_path = |arg: &str| -> Option<String> {
        Command::new("git")
            .args(["rev-parse", "--git-path", arg])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
    };

    if let Some(head) = git_path("HEAD") {
        println!("cargo:rerun-if-changed={head}");
    }

    // If HEAD is a symbolic ref (e.g. refs/heads/main), watch that file too
    if let Some(sym_ref) = Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
    {
        if let Some(ref_path) = git_path(&sym_ref) {
            println!("cargo:rerun-if-changed={ref_path}");
        }
    }
}

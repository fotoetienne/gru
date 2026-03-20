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

    // Rebuild when HEAD or refs change.
    // Use `git rev-parse --git-path` so this works correctly with worktrees.
    for path_arg in ["HEAD", "refs", "packed-refs"] {
        if let Some(path) = Command::new("git")
            .args(["rev-parse", "--git-path", path_arg])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

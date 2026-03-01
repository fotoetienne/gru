use once_cell::sync::OnceCell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Cached workspace instance initialized once on first successful call.
/// Uses OnceCell so transient failures don't get permanently cached.
static GLOBAL_WORKSPACE: OnceCell<Workspace> = OnceCell::new();

/// Manages the Gru workspace directory structure at `~/.gru`.
///
/// The workspace consists of:
/// - `root`: The main `.gru` directory
/// - `repos`: Cloned Git repositories
/// - `work`: Active working directories for minions
/// - `archive`: Completed or archived minion workspaces
/// - `state`: State files (e.g., minion ID counter)
// Allow dead code for now - workspace module will be integrated in future issues
#[allow(dead_code)]
pub struct Workspace {
    root: PathBuf,
    repos: PathBuf,
    work: PathBuf,
    archive: PathBuf,
    state: PathBuf,
}

#[allow(dead_code)]
impl Workspace {
    /// Returns a reference to the global cached Workspace instance.
    ///
    /// Initialized on first successful call. Subsequent calls return the same instance.
    /// If initialization fails, the next call will retry (transient errors are not cached).
    pub fn global() -> io::Result<&'static Workspace> {
        GLOBAL_WORKSPACE.get_or_try_init(Workspace::new)
    }

    /// Creates directories with appropriate permissions (0755 on Unix).
    fn create_dirs(dirs: &[&Path]) -> io::Result<()> {
        for dir in dirs {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o755);
                builder.recursive(true);
                builder.create(dir)?;
            }
            #[cfg(not(unix))]
            {
                fs::create_dir_all(dir)?;
            }
        }
        Ok(())
    }

    /// Builds and initializes a Workspace from a given root path.
    fn init(root: PathBuf) -> io::Result<Self> {
        let repos = root.join("repos");
        let work = root.join("work");
        let archive = root.join("archive");
        let state = root.join("state");

        Self::create_dirs(&[&root, &repos, &work, &archive, &state])?;

        Ok(Workspace {
            root,
            repos,
            work,
            archive,
            state,
        })
    }

    /// Creates a new workspace, initializing all required directories.
    ///
    /// Creates the directory structure at `~/.gru/` if it doesn't exist.
    /// Subsequent calls are idempotent and will succeed if directories already exist.
    ///
    /// On Unix systems, directories are created with permissions 0755.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The home directory cannot be determined
    /// - Directory creation fails due to permissions or I/O errors
    /// - Setting directory permissions fails (Unix only)
    pub fn new() -> io::Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Home directory not found"))?;
        Self::init(home.join(".gru"))
    }

    /// Creates a workspace with a custom root directory (for testing only).
    ///
    /// This constructor allows tests to use temporary directories instead of
    /// polluting the production `~/.gru/` directory.
    ///
    /// # Arguments
    ///
    /// * `root` - Custom root directory path (typically from `tempfile::tempdir()`)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Directory creation fails due to permissions or I/O errors
    /// - Setting directory permissions fails (Unix only)
    #[cfg(test)]
    pub fn new_with_root(root: PathBuf) -> io::Result<Self> {
        Self::init(root)
    }

    /// Returns a reference to the workspace root directory path (`~/.gru`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns a reference to the repos directory path (`~/.gru/repos`).
    pub fn repos(&self) -> &Path {
        &self.repos
    }

    /// Returns a reference to the work directory path (`~/.gru/work`).
    pub fn work(&self) -> &Path {
        &self.work
    }

    /// Returns a reference to the archive directory path (`~/.gru/archive`).
    pub fn archive(&self) -> &Path {
        &self.archive
    }

    /// Returns a reference to the state directory path (`~/.gru/state`).
    pub fn state(&self) -> &Path {
        &self.state
    }

    /// Returns the working directory path for a specific repo and branch name.
    ///
    /// This is the universal worktree path function that derives the path from the branch name.
    /// The worktree path always matches the branch name exactly for consistency.
    ///
    /// # Arguments
    ///
    /// * `repo` - Repository identifier (e.g., "owner/repo" or "owner_repo")
    /// * `branch_name` - Branch name (e.g., "gru/issue-123", "fix-auth-bug", "feature/new-api")
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let ws = Workspace::new()?;
    /// // Creates path: ~/.gru/work/owner/repo/gru/issue-123/
    /// let path = ws.work_dir("owner/repo", "gru/issue-123")?;
    ///
    /// // Creates path: ~/.gru/work/owner/repo/fix-auth-bug/
    /// let path = ws.work_dir("owner/repo", "fix-auth-bug")?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The repo contains backslashes or parent directory references
    /// - The branch_name contains backslashes or parent directory references
    /// - The resulting path would escape the workspace directory
    ///
    /// # Note
    ///
    /// This method validates inputs and computes the path, but does not create the directory.
    /// Forward slashes in repo names and branch names are allowed and will create nested directories.
    /// Remote prefixes (like "origin/") are automatically stripped from branch names.
    pub fn work_dir(&self, repo: &str, branch_name: &str) -> io::Result<PathBuf> {
        if repo.contains('\\') || repo.contains("..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Repository identifier '{}' contains invalid characters",
                    repo
                ),
            ));
        }

        // Strip remote prefix if present (origin/main → main)
        let local_branch = branch_name.strip_prefix("origin/").unwrap_or(branch_name);

        if local_branch.contains('\\') || local_branch.contains("..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Branch name '{}' contains invalid characters", local_branch),
            ));
        }

        let path = self.work.join(repo).join(local_branch);

        if !path.starts_with(&self.work) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Path escapes workspace directory",
            ));
        }

        Ok(path)
    }

    /// Returns the archive directory path for a specific minion.
    ///
    /// # Arguments
    ///
    /// * `minion_id` - Unique minion identifier
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The minion_id contains path separators or parent directory references
    /// - The resulting path would escape the workspace directory
    ///
    /// # Note
    ///
    /// This method validates inputs and computes the path, but does not create the directory.
    pub fn archive_dir(&self, minion_id: &str) -> io::Result<PathBuf> {
        if minion_id.contains('/') || minion_id.contains('\\') || minion_id.contains("..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Minion ID '{}' contains invalid characters", minion_id),
            ));
        }

        let path = self.archive.join(minion_id);

        if !path.starts_with(&self.archive) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Path escapes workspace directory",
            ));
        }

        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_creation() {
        let ws = Workspace::new().unwrap();
        assert!(ws.root().exists());
        assert!(ws.repos().exists());
        assert!(ws.work().exists());
        assert!(ws.archive().exists());
    }

    #[test]
    fn test_work_dir_path() {
        let ws = Workspace::new().unwrap();
        // Test with gru-created branch
        let work_path = ws.work_dir("myrepo", "gru/issue-123").unwrap();
        assert_eq!(work_path, ws.work().join("myrepo").join("gru/issue-123"));

        // Test with human-created branch
        let work_path2 = ws.work_dir("myrepo", "fix-auth-bug").unwrap();
        assert_eq!(work_path2, ws.work().join("myrepo").join("fix-auth-bug"));
    }

    #[test]
    fn test_archive_dir_path() {
        let ws = Workspace::new().unwrap();
        let archive_path = ws.archive_dir("minion-456").unwrap();
        assert_eq!(archive_path, ws.archive().join("minion-456"));
    }

    #[test]
    fn test_subsequent_calls_dont_fail() {
        let ws1 = Workspace::new().unwrap();
        let ws2 = Workspace::new().unwrap();
        assert_eq!(ws1.root(), ws2.root());
    }

    #[test]
    fn test_work_dir_rejects_path_traversal() {
        let ws = Workspace::new().unwrap();
        assert!(ws.work_dir("../../etc", "branch").is_err());
        assert!(ws.work_dir("repo", "../branch").is_err());
        assert!(ws.work_dir("repo\\subpath", "branch").is_err()); // Backslashes not allowed
    }

    #[test]
    fn test_work_dir_allows_forward_slashes() {
        let ws = Workspace::new().unwrap();
        // Forward slashes should be allowed in both repo names and branch names
        assert!(ws.work_dir("owner/repo", "gru/issue-123").is_ok());
        assert!(ws
            .work_dir("org/project/subproject", "feature/new-api")
            .is_ok());
    }

    #[test]
    fn test_archive_dir_rejects_path_traversal() {
        let ws = Workspace::new().unwrap();
        assert!(ws.archive_dir("../minion").is_err());
        assert!(ws.archive_dir("minion/subpath").is_err());
    }

    #[test]
    fn test_work_dir_accepts_dots_in_identifiers() {
        let ws = Workspace::new().unwrap();
        // Dots in repo names should be allowed (e.g., "owner.io/repo")
        assert!(ws.work_dir("owner.io", "branch").is_ok());
        assert!(ws.work_dir("my.repo", "branch").is_ok());
        // Dots in branch names should be allowed (e.g., version branches)
        assert!(ws.work_dir("repo", "release/1.2.3").is_ok());
        assert!(ws.work_dir("repo", "v2.0").is_ok());
    }

    #[test]
    fn test_archive_dir_accepts_dots_in_identifiers() {
        let ws = Workspace::new().unwrap();
        // Dots in minion IDs should be allowed
        assert!(ws.archive_dir("minion-1.2.3").is_ok());
        assert!(ws.archive_dir("v2.0").is_ok());
    }

    #[test]
    fn test_work_dir_strips_remote_prefix() {
        let ws = Workspace::new().unwrap();
        // Remote prefixes should be stripped automatically
        let work_path = ws.work_dir("owner/repo", "origin/main").unwrap();
        assert_eq!(work_path, ws.work().join("owner/repo").join("main"));

        let work_path2 = ws.work_dir("owner/repo", "origin/gru/issue-123").unwrap();
        assert_eq!(
            work_path2,
            ws.work().join("owner/repo").join("gru/issue-123")
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let ws = Workspace::new().unwrap();
        let metadata = fs::metadata(ws.root()).unwrap();
        let permissions = metadata.permissions();
        assert_eq!(permissions.mode() & 0o777, 0o755);
    }
}

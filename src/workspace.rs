use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Manages the Gru workspace directory structure at `~/.gru`.
///
/// The workspace consists of:
/// - `root`: The main `.gru` directory
/// - `repos`: Cloned Git repositories
/// - `work`: Active working directories for minions
/// - `archive`: Completed or archived minion workspaces
pub struct Workspace {
    root: PathBuf,
    repos: PathBuf,
    work: PathBuf,
    archive: PathBuf,
}

impl Workspace {
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

        let root = home.join(".gru");
        let repos = root.join("repos");
        let work = root.join("work");
        let archive = root.join("archive");

        for dir in [&root, &repos, &work, &archive] {
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

        Ok(Workspace {
            root,
            repos,
            work,
            archive,
        })
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

    /// Returns the working directory path for a specific repo and minion.
    ///
    /// # Arguments
    ///
    /// * `repo` - Repository identifier (e.g., "owner/repo")
    /// * `minion_id` - Unique minion identifier
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The repo or minion_id contains path separators or parent directory references
    /// - The resulting path would escape the workspace directory
    ///
    /// # Note
    ///
    /// This method validates inputs and computes the path, but does not create the directory.
    pub fn work_dir(&self, repo: &str, minion_id: &str) -> io::Result<PathBuf> {
        if repo.contains('/') || repo.contains('\\') || repo.contains("..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Repository identifier '{}' contains invalid characters",
                    repo
                ),
            ));
        }

        if minion_id.contains('/') || minion_id.contains('\\') || minion_id.contains("..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Minion ID '{}' contains invalid characters", minion_id),
            ));
        }

        let path = self.work.join(repo).join(minion_id);

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
        let work_path = ws.work_dir("myrepo", "minion-123").unwrap();
        assert_eq!(work_path, ws.work().join("myrepo").join("minion-123"));
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
        assert!(ws.work_dir("../../etc", "minion").is_err());
        assert!(ws.work_dir("repo", "../minion").is_err());
        assert!(ws.work_dir("repo/subpath", "minion").is_err());
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
        assert!(ws.work_dir("owner.io", "minion").is_ok());
        assert!(ws.work_dir("my.repo", "minion").is_ok());
        // Dots in minion IDs should be allowed (e.g., version numbers)
        assert!(ws.work_dir("repo", "minion-1.2.3").is_ok());
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
    #[cfg(unix)]
    fn test_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let ws = Workspace::new().unwrap();
        let metadata = fs::metadata(ws.root()).unwrap();
        let permissions = metadata.permissions();
        assert_eq!(permissions.mode() & 0o777, 0o755);
    }
}

use std::fs;
use std::io;
use std::path::PathBuf;

pub struct Workspace {
    pub root: PathBuf,
    pub repos: PathBuf,
    pub work: PathBuf,
    pub archive: PathBuf,
}

impl Workspace {
    pub fn new() -> io::Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Home directory not found"))?;

        let root = home.join(".gru");
        let repos = root.join("repos");
        let work = root.join("work");
        let archive = root.join("archive");

        for dir in [&root, &repos, &work, &archive] {
            fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = fs::Permissions::from_mode(0o755);
                fs::set_permissions(dir, permissions)?;
            }
        }

        Ok(Workspace {
            root,
            repos,
            work,
            archive,
        })
    }

    pub fn work_dir(&self, repo: &str, minion_id: &str) -> PathBuf {
        self.work.join(repo).join(minion_id)
    }

    pub fn archive_dir(&self, minion_id: &str) -> PathBuf {
        self.archive.join(minion_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_creation() {
        let ws = Workspace::new().unwrap();
        assert!(ws.root.exists());
        assert!(ws.repos.exists());
        assert!(ws.work.exists());
        assert!(ws.archive.exists());
    }

    #[test]
    fn test_work_dir_path() {
        let ws = Workspace::new().unwrap();
        let work_path = ws.work_dir("myrepo", "minion-123");
        assert!(work_path.to_string_lossy().contains("myrepo"));
        assert!(work_path.to_string_lossy().contains("minion-123"));
    }

    #[test]
    fn test_archive_dir_path() {
        let ws = Workspace::new().unwrap();
        let archive_path = ws.archive_dir("minion-456");
        assert!(archive_path.to_string_lossy().contains("minion-456"));
    }

    #[test]
    fn test_subsequent_calls_dont_fail() {
        let ws1 = Workspace::new().unwrap();
        let ws2 = Workspace::new().unwrap();
        assert_eq!(ws1.root, ws2.root);
    }
}

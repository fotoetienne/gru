use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use once_cell::sync::Lazy;

use crate::workspace::Workspace;

/// Cached workspace instance for production state directory access.
/// Initialized once on first use to avoid repeated directory creation checks.
static WORKSPACE: Lazy<io::Result<Workspace>> = Lazy::new(Workspace::new);

/// Represents a Minion workspace for working on a specific GitHub issue
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Minion {
    pub id: String,
    pub repo: String,
    pub issue: String,
    pub workspace_path: PathBuf,
}

/// Generates a unique Minion ID using a monotonic counter
///
/// IDs are formatted as M<base36> where the counter is encoded in base36:
/// M000, M001, M002, ..., M00z, M010, ..., M0zz, M100, ...
///
/// Note: IDs use lowercase letters (a-z) for improved readability. Legacy IDs
/// from earlier versions may contain uppercase letters (A-Z) for counter values 10-35.
///
/// The counter is stored in `~/.gru/state/next_id.txt` and uses file locking
/// to ensure thread-safety and atomicity.
///
/// # Arguments
///
/// * `state_dir` - Optional custom state directory path. If None, uses `~/.gru/state/`.
///   This parameter is primarily for testing with isolated temp directories.
#[allow(dead_code)]
pub fn generate_minion_id_with_state(state_dir: Option<&Path>) -> io::Result<String> {
    let state_path = if let Some(custom_dir) = state_dir {
        // Test path: use provided directory and ensure it exists
        fs::create_dir_all(custom_dir)?;
        custom_dir.to_path_buf()
    } else {
        // Production path: use cached workspace (directory already created by Workspace::new)
        let workspace = WORKSPACE.as_ref().map_err(|e| {
            io::Error::new(e.kind(), format!("Failed to initialize workspace: {}", e))
        })?;
        workspace.state().to_path_buf()
    };

    let counter_path = state_path.join("next_id.txt");

    // Open or create the counter file with exclusive access
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&counter_path)?;

    // Lock the file for exclusive access
    file.lock_exclusive()?;

    // Read current counter value
    let metadata = file.metadata()?;
    let counter: u64 = if metadata.len() == 0 {
        0 // Empty file means new installation - start at 0 for M000
    } else {
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;

        if contents.trim().is_empty() {
            0 // Start at 0 for new installations (first ID will be M000)
        } else {
            contents.trim().parse().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Corrupted counter file: {}", e),
                )
            })?
        }
    };

    // Generate the ID
    let id = format!("M{}", to_base36(counter));

    // Increment and write back
    let next_counter = counter + 1;
    file.set_len(0)?;
    file.seek(io::SeekFrom::Start(0))?;
    write!(file, "{}", next_counter)?;
    file.flush()?;

    // Unlock the file (happens automatically when file is dropped on both Unix and Windows)

    Ok(id)
}

/// Generates a unique Minion ID using the default production state directory.
///
/// This is a convenience wrapper around `generate_minion_id_with_state(None)`.
#[allow(dead_code)]
pub fn generate_minion_id() -> io::Result<String> {
    generate_minion_id_with_state(None)
}

/// Converts a number to base36 with minimum 3 digits (padded with zeros)
fn to_base36(mut num: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    if num == 0 {
        return "000".to_string();
    }

    let mut result = Vec::new();
    while num > 0 {
        result.push(DIGITS[(num % 36) as usize] as char);
        num /= 36;
    }

    // Pad to minimum 3 digits
    while result.len() < 3 {
        result.push('0');
    }

    result.reverse();
    result.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::thread;

    #[test]
    fn test_base36_encoding() {
        assert_eq!(to_base36(0), "000");
        assert_eq!(to_base36(1), "001");
        assert_eq!(to_base36(2), "002");
        assert_eq!(to_base36(10), "00a");
        assert_eq!(to_base36(35), "00z");
        assert_eq!(to_base36(36), "010");
        assert_eq!(to_base36(71), "01z");
        assert_eq!(to_base36(1295), "0zz");
        assert_eq!(to_base36(1296), "100");
    }

    #[test]
    fn test_unique_ids() {
        let temp_dir = tempfile::tempdir().unwrap();
        let id1 = generate_minion_id_with_state(Some(temp_dir.path())).unwrap();
        let id2 = generate_minion_id_with_state(Some(temp_dir.path())).unwrap();
        assert_ne!(id1, id2);
        assert!(id1.starts_with("M"));
        assert!(id2.starts_with("M"));
    }

    #[test]
    fn test_id_format() {
        let temp_dir = tempfile::tempdir().unwrap();
        let id = generate_minion_id_with_state(Some(temp_dir.path())).unwrap();
        assert!(id.starts_with("M"));
        assert!(id.len() >= 4); // M + at least 3 digits
                                // Check that all characters after M are valid lowercase base36
        for c in id.chars().skip(1) {
            assert!(c.is_ascii_alphanumeric());
            if c.is_ascii_alphabetic() {
                assert!(
                    c.is_ascii_lowercase(),
                    "Minion ID should only contain lowercase letters, found: {}",
                    c
                );
            }
        }
    }

    #[test]
    fn test_concurrent_id_generation() {
        use std::sync::Arc;
        let temp_dir = Arc::new(tempfile::tempdir().unwrap());
        let mut handles = vec![];
        let mut ids = HashSet::new();

        // Generate IDs concurrently
        for _ in 0..5 {
            let temp_dir_clone = Arc::clone(&temp_dir);
            let handle = thread::spawn(move || {
                generate_minion_id_with_state(Some(temp_dir_clone.path())).unwrap()
            });
            handles.push(handle);
        }

        // Collect all IDs
        for handle in handles {
            let id = handle.join().unwrap();
            ids.insert(id);
        }

        // All IDs should be unique
        assert_eq!(ids.len(), 5);
    }

    #[test]
    fn test_minion_struct() {
        let minion = Minion {
            id: "M001".to_string(),
            repo: "fotoetienne/gru".to_string(),
            issue: "123".to_string(),
            workspace_path: PathBuf::from("/tmp/test"),
        };

        assert_eq!(minion.id, "M001");
        assert_eq!(minion.repo, "fotoetienne/gru");
        assert_eq!(minion.issue, "123");
    }
}

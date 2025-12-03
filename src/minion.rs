use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::PathBuf;

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
/// M001, M002, ..., M00Z, M010, ..., M0ZZ, M100, ...
///
/// The counter is stored in `~/.gru/state/next_id.txt` and uses file locking
/// to ensure thread-safety and atomicity.
#[allow(dead_code)]
pub fn generate_minion_id() -> io::Result<String> {
    let state_dir = dirs::data_local_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Local data directory not found"))?
        .join("gru")
        .join("state");

    fs::create_dir_all(&state_dir)?;

    let counter_path = state_dir.join("next_id.txt");

    // Open or create the counter file with exclusive access
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&counter_path)?;

    // Lock the file (platform-specific)
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        unsafe {
            if libc::flock(file.as_raw_fd(), libc::LOCK_EX) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK};
        use windows_sys::Win32::System::IO::OVERLAPPED;

        unsafe {
            let mut overlapped: OVERLAPPED = std::mem::zeroed();
            if LockFileEx(
                file.as_raw_handle() as _,
                LOCKFILE_EXCLUSIVE_LOCK,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
    }

    // Read current counter value
    let metadata = file.metadata()?;
    let counter: u64 = if metadata.len() == 0 {
        1 // Empty file means new installation
    } else {
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;

        if contents.trim().is_empty() {
            1 // Start at 1 for new installations
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

    // Unlock the file (happens automatically when file is dropped on Unix)

    Ok(id)
}

/// Converts a number to base36 with minimum 3 digits (padded with zeros)
fn to_base36(mut num: u64) -> String {
    const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

    if num == 0 {
        return "001".to_string();
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
        assert_eq!(to_base36(1), "001");
        assert_eq!(to_base36(2), "002");
        assert_eq!(to_base36(10), "00A");
        assert_eq!(to_base36(35), "00Z");
        assert_eq!(to_base36(36), "010");
        assert_eq!(to_base36(71), "01Z");
        assert_eq!(to_base36(1295), "0ZZ");
        assert_eq!(to_base36(1296), "100");
    }

    #[test]
    fn test_unique_ids() {
        let id1 = generate_minion_id().unwrap();
        let id2 = generate_minion_id().unwrap();
        assert_ne!(id1, id2);
        assert!(id1.starts_with("M"));
        assert!(id2.starts_with("M"));
    }

    #[test]
    fn test_id_format() {
        let id = generate_minion_id().unwrap();
        assert!(id.starts_with("M"));
        assert!(id.len() >= 4); // M + at least 3 digits
                                // Check that all characters after M are valid base36
        for c in id.chars().skip(1) {
            assert!(c.is_ascii_alphanumeric());
        }
    }

    #[test]
    fn test_concurrent_id_generation() {
        let mut handles = vec![];
        let mut ids = HashSet::new();

        // Generate IDs concurrently
        for _ in 0..5 {
            let handle = thread::spawn(|| generate_minion_id().unwrap());
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

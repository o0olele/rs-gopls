use std::path::Path;

use crate::semantic::shared_disk::FileObservation;

/// Observations from a single request. Windows directory enumeration provides
/// the same timestamps and length as metadata for regular files in one read.
#[derive(Default)]
pub(in crate::semantic) struct DirectoryObservations {
    #[cfg(windows)]
    directories: std::collections::HashMap<std::path::PathBuf, Option<DirectoryEntries>>,
}

#[cfg(windows)]
type DirectoryEntries = std::collections::HashMap<std::ffi::OsString, FileObservation>;

impl DirectoryObservations {
    pub(in crate::semantic) fn observe(&mut self, path: &Path) -> Option<FileObservation> {
        #[cfg(windows)]
        if let Some((parent, name)) = path.parent().zip(path.file_name()) {
            if !self.directories.contains_key(parent) {
                // Large or unusual directories fall back to observing the file.
                if self.directories.len() == 8 {
                    self.directories.clear();
                }
                self.directories
                    .insert(parent.to_owned(), directory_entries(parent));
            }
            if let Some(observation) = self.directories[parent]
                .as_ref()
                .and_then(|entries| entries.get(name))
            {
                return Some(*observation);
            }
        }
        FileObservation::read(path).ok()
    }
}

#[cfg(windows)]
fn directory_entries(path: &Path) -> Option<DirectoryEntries> {
    use std::os::windows::fs::MetadataExt;

    const MAX_ENTRIES: usize = 4096;
    const REPARSE_POINT: u32 = 0x400;
    let mut entries = DirectoryEntries::new();
    for (index, entry) in path.read_dir().ok()?.enumerate() {
        if index == MAX_ENTRIES {
            return None;
        }
        let entry = entry.ok()?;
        let name = entry.file_name();
        if Path::new(&name)
            .extension()
            .is_some_and(|extension| extension == "go")
            && let Ok(metadata) = entry.metadata()
            && metadata.is_file()
            && metadata.file_attributes() & REPARSE_POINT == 0
        {
            entries.insert(name, FileObservation::from_metadata(metadata));
        }
    }
    Some(entries)
}

#[cfg(windows)]
mod change_watch {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::SystemTime;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FindFirstChangeNotificationW(path: *const u16, subtree: i32, filter: u32) -> isize;
        fn FindNextChangeNotification(handle: isize) -> i32;
        fn FindCloseChangeNotification(handle: isize) -> i32;
        fn WaitForSingleObject(handle: isize, timeout_ms: u32) -> u32;
    }

    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;
    // File name, directory name, size, last write, creation.
    const CONTENT_CHANGES: u32 = 0x5b;

    /// Rearming and consuming a signal are serialized so two callers cannot
    /// disagree about the revision that a signaled handle invalidates.
    pub(in crate::semantic) struct DirectoryWatch {
        handle: isize,
        path: PathBuf,
        created: SystemTime,
        generation: Mutex<Option<u64>>,
    }

    impl DirectoryWatch {
        pub(in crate::semantic) fn new(path: &Path) -> Option<Self> {
            let created = path.symlink_metadata().ok()?.created().ok()?;
            let wide_path: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
            // SAFETY: The nul-terminated buffer lives for the call.
            let handle =
                unsafe { FindFirstChangeNotificationW(wide_path.as_ptr(), 1, CONTENT_CHANGES) };
            (handle != -1).then(|| Self {
                handle,
                path: path.to_owned(),
                created,
                generation: Mutex::new(Some(0)),
            })
        }

        pub(in crate::semantic) fn generation(&self) -> Option<u64> {
            let mut generation = self.generation.lock().unwrap();
            let current = (*generation)?;
            if self.path.symlink_metadata().ok().and_then(|metadata| {
                let direct_directory = metadata.is_dir() && !metadata.is_symlink();
                direct_directory.then(|| metadata.created().ok()).flatten()
            }) != Some(self.created)
            {
                *generation = None;
                return None;
            }
            // SAFETY: The handle remains open for the lifetime of self.
            match unsafe { WaitForSingleObject(self.handle, 0) } {
                WAIT_TIMEOUT => Some(current),
                WAIT_OBJECT_0 => {
                    *generation = if unsafe { FindNextChangeNotification(self.handle) } != 0 {
                        current.checked_add(1)
                    } else {
                        None
                    };
                    *generation
                }
                _ => {
                    *generation = None;
                    None
                }
            }
        }
    }

    impl Drop for DirectoryWatch {
        fn drop(&mut self) {
            // SAFETY: This instance owns the change notification handle.
            unsafe { FindCloseChangeNotification(self.handle) };
        }
    }
}

#[cfg(windows)]
pub(in crate::semantic) use change_watch::DirectoryWatch;

#[cfg(not(windows))]
pub(in crate::semantic) struct DirectoryWatch;

#[cfg(not(windows))]
impl DirectoryWatch {
    pub(in crate::semantic) fn new(_path: &Path) -> Option<Self> {
        None
    }

    pub(in crate::semantic) fn generation(&self) -> Option<u64> {
        None
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::time::{Duration, Instant};

    use crate::test_support::TestProject;

    use super::*;

    #[test]
    fn native_subtree_watch_reports_changes_and_rearms() {
        let project = TestProject::new();
        let path = project.write("src/lib/value.go", "package lib\n");
        let watch = DirectoryWatch::new(&project.root).unwrap();
        assert_eq!(watch.generation(), Some(0));
        for (expected, contents) in [
            (1, "package lib\nvar First int\n"),
            (2, "package lib\nvar Second string\n"),
        ] {
            std::fs::write(&path, contents).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let generation = watch.generation().unwrap();
                if generation == expected {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "watcher missed generation {expected}"
                );
                std::thread::yield_now();
            }
        }
    }
}

//! User-requested cleanup of reconstructible disk data, off the UI thread.
use std::sync::mpsc::{self, Receiver};

#[derive(Default)]
pub struct Report {
    pub files: usize,
    pub bytes: u64,
    pub failed: usize,
}

pub fn start() -> Receiver<Report> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("reimu-cache-cleanup".into()).spawn(move || {
        let mut report = Report::default();
        // Only application-owned, hashed cache files; never recurse into arbitrary directories.
        if let Some(root) = super::library::config_dir() {
            for (folder, extension) in [("playlist-cache", "json"), ("covers", "bin")] {
                let dir = root.join(folder);
                // Do not follow a directory junction/symlink substituted for a cache folder.
                if std::fs::symlink_metadata(&dir).map_or(true, |m| {
                    #[cfg(windows)] {
                        use std::os::windows::fs::MetadataExt;
                        !m.is_dir() || m.file_attributes() & 0x400 != 0
                    }
                    #[cfg(not(windows))] { !m.is_dir() || m.file_type().is_symlink() }
                }) { continue; }
                let Ok(entries) = std::fs::read_dir(&dir) else { continue; };
                for entry in entries.flatten() {
                    if !entry.file_type().is_ok_and(|kind| kind.is_file()) { continue; }
                    let path = entry.path();
                    let stem = path.file_stem().and_then(|v| v.to_str()).unwrap_or_default();
                    if path.extension().and_then(|v| v.to_str()) != Some(extension)
                        || stem.len() != 16 || !stem.bytes().all(|b| b.is_ascii_hexdigit()) {
                        continue;
                    }
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    match std::fs::remove_file(&path) {
                        Ok(()) => { report.files += 1; report.bytes += size; }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => report.failed += 1,
                    }
                }
            }
        }
        let scratch = crate::music::online_playback::sweep_stale_scratch();
        report.files += scratch.files_removed as usize;
        report.bytes += scratch.bytes_freed;
        report.failed += scratch.dirs_failed as usize;
        let _ = tx.send(report);
    });
    rx
}

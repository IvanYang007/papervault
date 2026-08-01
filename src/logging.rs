use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{error, info, warn};

/// Retention window for session log files.
const LOG_RETENTION: Duration = Duration::from_secs(3 * 24 * 60 * 60);

/// Directory holding all papervault logs and crash records.
pub fn log_dir() -> PathBuf {
    dirs_next::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("papervault")
}

/// Per-session log file: `papervault-YYYYMMDD-HHMMSS-fff.log`.
/// Milliseconds make rapid restarts produce distinct files, so no
/// session ever truncates another's log.
pub fn session_log_path(dir: &Path, now: &chrono::DateTime<chrono::Local>) -> PathBuf {
    dir.join(format!("papervault-{}.log", now.format("%Y%m%d-%H%M%S-%3f")))
}

/// Retention sweep: delete session log files (and the legacy single
/// `papervault.log`) whose mtime is strictly older than `max_age`.
/// One cheap directory scan, run once at startup — no timers, no
/// background threads, no ongoing resource use. Files younger than
/// `max_age` (including the active session) are never touched.
pub fn sweep_old_logs(dir: &Path, now: SystemTime, max_age: Duration) -> io::Result<usize> {
    let mut removed = 0usize;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let name = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        let is_session_log = name.starts_with("papervault-") && name.ends_with(".log");
        let is_legacy_log = name == "papervault.log";
        if !is_session_log && !is_legacy_log {
            continue;
        }
        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        // Only strictly-older files are removed — a file exactly at the
        // retention age survives, and clock skew keeps files safe.
        let too_old = now
            .duration_since(modified)
            .map(|age| age > max_age)
            .unwrap_or(false);
        if too_old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Append one record to a file — used for crash.log so every panic is
/// preserved instead of overwriting the previous one.
pub fn append_record(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(text.as_bytes())?;
    f.write_all(b"\n")?;
    // Crash records must survive process death — flush to disk.
    f.sync_all()?;
    Ok(())
}

/// Initialize logging:
/// 1. Retention sweep (3 days, one directory scan, no timers).
/// 2. Open a fresh per-session log file (never truncates old sessions).
/// 3. Install tracing (level from RUST_LOG, default `info`).
/// 4. Install a panic hook that logs AND appends to crash.log with the
///    session id, so a crash is always traceable to its session log.
///
/// Returns the session id (timestamp part of the log file name).
pub fn init() -> String {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);

    // Sweep BEFORE opening the session file so the active file is never a
    // candidate; the sweep result is logged after tracing is up.
    let swept = sweep_old_logs(&dir, SystemTime::now(), LOG_RETENTION);

    let now = chrono::Local::now();
    let log_path = session_log_path(&dir, &now);
    let session_id = log_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.trim_start_matches("papervault-").to_string())
        .unwrap_or_else(|| now.format("%Y%m%d-%H%M%S-%3f").to_string());

    let log_file = std::fs::File::create(&log_path).expect("failed to create log file");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::sync::Mutex::new(log_file))
        .init();

    match swept {
        Ok(0) => {}
        Ok(n) => info!("Retention sweep: removed {} old log file(s) (keep 3 days)", n),
        Err(e) => warn!("Retention sweep failed: {}", e),
    }

    let session_id_for_hook = session_id.clone();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info.to_string();
        let tid = std::thread::current().id();
        eprintln!("!!! PANIC in thread {:?}: {}", tid, msg);
        error!("Panic in thread {:?}: {}", tid, msg);
        if let Some(crash_dir) = dirs_next::data_local_dir() {
            let crash_path = crash_dir.join("papervault").join("crash.log");
            if let Some(parent) = crash_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = append_record(
                &crash_path,
                &format!(
                    "Panic at {} (session {}): {}",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    session_id_for_hook,
                    msg
                ),
            );
        }
        eprintln!("FATAL: {}", msg);
    }));

    info!(
        "Session started (session_id={}) — log: {}",
        session_id,
        log_path.display()
    );
    session_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn set_age(path: &Path, age: Duration) {
        let old = SystemTime::now() - age;
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old)).unwrap();
    }

    const THREE_DAYS: Duration = Duration::from_secs(3 * 24 * 60 * 60);

    #[test]
    fn sweep_removes_only_old_papervault_logs() {
        let dir = tempfile::TempDir::new().unwrap();
        let old_session = dir.path().join("papervault-20260725-100000-000.log");
        let new_session = dir.path().join("papervault-20260801-200000-000.log");
        let legacy = dir.path().join("papervault.log");
        let unrelated = dir.path().join("notes.txt");
        std::fs::write(&old_session, "a").unwrap();
        std::fs::write(&new_session, "b").unwrap();
        std::fs::write(&legacy, "c").unwrap();
        std::fs::write(&unrelated, "d").unwrap();
        set_age(&old_session, Duration::from_secs(4 * 24 * 60 * 60));
        set_age(&legacy, Duration::from_secs(4 * 24 * 60 * 60));

        let n = sweep_old_logs(dir.path(), SystemTime::now(), THREE_DAYS).unwrap();
        assert_eq!(n, 2, "exactly the two old papervault logs must be removed");
        assert!(!old_session.exists());
        assert!(!legacy.exists());
        assert!(new_session.exists(), "fresh session log must be kept");
        assert!(unrelated.exists(), "non-log files must never be touched");
    }

    #[test]
    fn sweep_keeps_files_younger_than_retention() {
        let dir = tempfile::TempDir::new().unwrap();
        // 5 minutes inside the retention window — must survive.
        let near_boundary = dir.path().join("papervault-20260729-000000-000.log");
        std::fs::write(&near_boundary, "x").unwrap();
        set_age(&near_boundary, THREE_DAYS - Duration::from_secs(300));

        let n = sweep_old_logs(dir.path(), SystemTime::now(), THREE_DAYS).unwrap();
        assert_eq!(n, 0);
        assert!(near_boundary.exists());
    }

    #[test]
    fn sweep_missing_dir_is_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let n = sweep_old_logs(
            &dir.path().join("does-not-exist"),
            SystemTime::now(),
            THREE_DAYS,
        )
        .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn append_record_accumulates_instead_of_overwriting() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("crash.log");
        append_record(&p, "first crash").unwrap();
        append_record(&p, "second crash").unwrap();
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(content.contains("first crash"), "content: {}", content);
        assert!(content.contains("second crash"), "content: {}", content);
    }

    #[test]
    fn session_log_path_is_unique_per_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let t1 = chrono::Local
            .with_ymd_and_hms(2026, 8, 1, 12, 0, 0)
            .unwrap();
        let p1 = session_log_path(dir.path(), &t1);
        let name = p1.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("papervault-20260801-120000-"), "name: {}", name);
        assert!(name.ends_with(".log"));

        // Two sessions in the same second still produce distinct files.
        let t2 = t1 + chrono::Duration::milliseconds(5);
        let p2 = session_log_path(dir.path(), &t2);
        assert_ne!(p1, p2, "millisecond component must disambiguate sessions");
    }
}

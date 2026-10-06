//! One log file per session: `$XDG_STATE_HOME/fleet-lsp/<lang>/<utc>-<pid>.log`.
//!
//! No shared file, so parallel sessions never interleave mid-line and there
//! is no rotation race. A session deletes its own language's logs older than
//! 14 days when it starts (unlinking a file another session holds open is
//! harmless on Unix). Logging never fails the relay: a write error is
//! dropped.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const KEEP: Duration = Duration::from_secs(14 * 24 * 3600);

/// `$XDG_STATE_HOME/fleet-lsp`, else `~/.local/state/fleet-lsp`.
pub(crate) fn log_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("fleet-lsp")
}

#[derive(Debug)]
pub(crate) struct Log {
    file: Option<File>,
    path: PathBuf,
}

impl Log {
    pub(crate) fn open(lang: &str) -> Log {
        let dir = log_dir().join(lang);
        let _ = fs::create_dir_all(&dir);
        prune(&dir, SystemTime::now());
        let now = SystemTime::now();
        let name = format!("{}-{}.log", compact(now), std::process::id());
        let path = dir.join(name);
        let file = File::options().create(true).append(true).open(&path).ok();
        Log { file, path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn line(&mut self, text: &str) {
        if let Some(f) = &mut self.file {
            let line = format!(
                "{} {}\n",
                rfc3339(SystemTime::now()),
                text.replace('\n', " | ")
            );
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// The newest session log under `dir` (any language), for `doctor`.
pub(crate) fn newest(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for lang in fs::read_dir(dir).ok()?.flatten() {
        for f in fs::read_dir(lang.path()).into_iter().flatten().flatten() {
            let t = f.metadata().and_then(|m| m.modified()).ok();
            if let Some(t) = t {
                if best.as_ref().map_or(true, |(b, _)| t > *b) {
                    best = Some((t, f.path()));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn prune(dir: &Path, now: SystemTime) {
    for f in fs::read_dir(dir).into_iter().flatten().flatten() {
        let old = f
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > KEEP);
        if old && f.path().extension().is_some_and(|e| e == "log") {
            let _ = fs::remove_file(f.path());
        }
    }
}

/// A path for people: `~/…` when under `$HOME`.
pub(crate) fn tilde(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

/// `2026-10-07T01:02:03.456Z`
pub(crate) fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (y, mo, da, h, mi, s) = civil(d.as_secs());
    format!(
        "{y:04}-{mo:02}-{da:02}T{h:02}:{mi:02}:{s:02}.{:03}Z",
        d.subsec_millis()
    )
}

/// `20261007T010203Z`, for file names that sort by time.
fn compact(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (y, mo, da, h, mi, s) = civil(d.as_secs());
    format!("{y:04}{mo:02}{da:02}T{h:02}{mi:02}{s:02}Z")
}

/// Seconds since the epoch to UTC civil time (Howard Hinnant's algorithm).
fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_time_known_points() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // 2026-10-07T01:02:03Z
        let t = UNIX_EPOCH + Duration::from_secs(1_791_334_923);
        assert_eq!(rfc3339(t), "2026-10-07T01:02:03.000Z");
        assert_eq!(compact(t), "20261007T010203Z");
        // A leap day.
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(rfc3339(leap), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn tilde_shortens_home() {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        assert_eq!(tilde(&home.join("x/y")), "~/x/y");
        assert_eq!(tilde(Path::new("/elsewhere")), "/elsewhere");
    }
}

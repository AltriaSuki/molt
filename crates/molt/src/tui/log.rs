//! Read only a bounded tail of the service log; never follow a log symlink.
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::ensure;

const BYTES: u64 = 64 * 1024;
const LINES: usize = 200;

pub fn tail(path: &Path) -> anyhow::Result<Vec<String>> {
    let mut file =
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "service log is not a regular file");
    let start = meta.len().saturating_sub(BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(BYTES).read_to_end(&mut bytes)?;
    if start > 0 {
        let first = bytes.iter().position(|&b| b == b'\n').map_or(bytes.len(), |i| i + 1);
        bytes.drain(..first);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<_> = text.lines().rev().take(LINES).map(str::to_owned).collect();
    lines.reverse();
    if start > 0 || text.lines().count() > LINES {
        lines.insert(0, "(showing the end of the service log)".into());
    }
    if lines.is_empty() {
        lines.push("the service log is empty".into());
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_logs_have_bounded_tails_and_symlinks_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("tui.log");
        let contents =
            format!("{}\n{}", "x".repeat(100_000), (0..300).map(|i| format!("line {i}\n")).collect::<String>());
        std::fs::write(&path, contents).unwrap();
        let lines = tail(&path).unwrap();
        assert_eq!(lines.len(), LINES + 1);
        assert_eq!(lines.last().unwrap(), "line 299");
        assert!(!lines.iter().any(|line| line == "line 0"));
        let link = root.path().join("linked.log");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(tail(&link).is_err());
    }
}

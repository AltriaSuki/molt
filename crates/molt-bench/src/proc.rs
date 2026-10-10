//! Running a task's test commands: with bash, in a clean environment and an
//! empty home directory of their own, in a process group of their own that
//! is killed when they finish or time out.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Bytes kept from the end of each of a command's output streams.
const KEEP: usize = 16 * 1024;
/// How long a finished command's output pipes may stay open, held by a
/// process that left its group, before they are abandoned.
const GRACE: Duration = Duration::from_secs(2);

/// Variables a command gets from the benchmark's own environment, when they
/// are set there: where programs are, whose they are, the locale and
/// toolchain roots. Proxies, `PYTHONPATH`, virtualenvs and the like are not
/// passed, so a check runs the same on any machine and reaches no network.
const PASSED_ENV: &[&str] = &[
    "PATH",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TZ",
    "TMPDIR",
    "GOROOT",
    "JAVA_HOME",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Set for every command, over whatever the environment says. Python does
/// not read the user's site-packages or write bytecode next to the code.
const FIXED_ENV: &[(&str, &str)] = &[
    ("PYTHONNOUSERSITE", "1"),
    ("PYTHONDONTWRITEBYTECODE", "1"),
    ("CI", "1"),
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// How a command ended.
#[derive(Clone, Debug, PartialEq)]
pub struct Ran {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub duration: Duration,
    /// The end of its output: stdout, then stderr.
    pub output: String,
}

impl Ran {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }

    /// How it ended, in a few words.
    pub fn ending(&self) -> String {
        match (self.timed_out, self.exit_code, self.signal) {
            (true, ..) => format!("timed out after {}s", self.duration.as_secs()),
            (false, Some(code), _) => format!("exit code {code}"),
            (false, None, Some(signal)) => format!("killed by signal {signal}"),
            (false, None, None) => "ended without a status".to_owned(),
        }
    }
}

/// Variables that make `home` a process's home directory, with the caches
/// and settings toolchains keep there (Cargo's, Go's, the XDG ones), so
/// nothing it writes there reaches another run. Rustup's toolchains are
/// still found where they are installed.
pub fn home_env(home: &Path) -> Vec<(String, String)> {
    let at = |rel: &str| home.join(rel).display().to_string();
    let mut env: Vec<(String, String)> = [
        ("HOME", home.display().to_string()),
        ("XDG_CACHE_HOME", at(".cache")),
        ("XDG_CONFIG_HOME", at(".config")),
        ("XDG_DATA_HOME", at(".local/share")),
        ("XDG_STATE_HOME", at(".local/state")),
        ("CARGO_HOME", at(".cargo")),
        ("GOPATH", at("go")),
        ("GOCACHE", at(".cache/go-build")),
        ("GOMODCACHE", at("go/pkg/mod")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    if let Some(rustup) = rustup_home() {
        env.push(("RUSTUP_HOME".into(), rustup.display().to_string()));
    }
    env
}

/// Where rustup keeps its toolchains, when it is installed.
fn rustup_home() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("RUSTUP_HOME").filter(|d| !d.is_empty()) {
        return Some(dir.into());
    }
    let dir = Path::new(&std::env::var_os("HOME")?).join(".rustup");
    dir.is_dir().then_some(dir)
}

/// The variables a test command gets: [`PASSED_ENV`] as set here, an empty
/// home directory `home` ([`home_env`]), and [`FIXED_ENV`]. API keys and
/// anything else are left out.
pub fn clean_env(home: &Path) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> =
        PASSED_ENV.iter().filter_map(|name| std::env::var(name).ok().map(|value| (name.to_string(), value))).collect();
    env.extend(home_env(home));
    env.extend(FIXED_ENV.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    env
}

/// Run `command` with `bash -c` in `dir` with [`clean_env`], a new empty
/// home directory, and no stdin. The command and everything it starts are
/// killed at `timeout`, and when the command itself exits.
pub async fn shell(dir: &Path, command: &str, timeout: Duration) -> io::Result<Ran> {
    let home = tempfile::Builder::new().prefix("molt-bench-home-").tempdir()?;
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .env_clear()
        .envs(clean_env(home.path()))
        .kill_on_drop(true);
    let started = Instant::now();
    let mut child = cmd.spawn()?;
    let group = Group(child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()));
    let stdout = Arc::new(Mutex::new(Tail::default()));
    let stderr = Arc::new(Mutex::new(Tail::default()));
    let readers = [
        tokio::spawn(drain(child.stdout.take(), stdout.clone())),
        tokio::spawn(drain(child.stderr.take(), stderr.clone())),
    ];

    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => (status, false),
        Err(_) => {
            group.kill();
            (child.wait().await, true)
        }
    };
    let duration = started.elapsed();
    group.kill();
    let status = status?;
    for reader in readers {
        let abort = reader.abort_handle();
        if tokio::time::timeout(GRACE, reader).await.is_err() {
            abort.abort();
        }
    }
    let mut output = take(&stdout);
    output.extend(take(&stderr));
    Ok(Ran {
        exit_code: status.code(),
        signal: status.signal(),
        timed_out,
        duration,
        output: String::from_utf8_lossy(&output).into_owned(),
    })
}

/// A command's process group, killed when this is dropped.
struct Group(Option<libc::pid_t>);

impl Group {
    fn kill(&self) {
        if let Some(pgid) = self.0 {
            // SAFETY: killpg only sends a signal; an empty group gives ESRCH, which is fine.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The last [`KEEP`] bytes of a stream, with a marker when more came before.
#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    dropped: u64,
}

impl Tail {
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > 2 * KEEP {
            let excess = self.bytes.len() - KEEP;
            self.bytes.drain(..excess);
            self.dropped += excess as u64;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        let excess = self.bytes.len().saturating_sub(KEEP);
        self.bytes.drain(..excess);
        self.dropped += excess as u64;
        if self.dropped == 0 {
            return self.bytes;
        }
        let mut out = format!("[... {} bytes omitted ...]\n", self.dropped).into_bytes();
        out.extend(self.bytes);
        out
    }
}

async fn drain(pipe: Option<impl AsyncRead + Unpin>, into: Arc<Mutex<Tail>>) {
    let Some(mut pipe) = pipe else { return };
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => into.lock().unwrap_or_else(PoisonError::into_inner).push(&buf[..n]),
        }
    }
}

fn take(tail: &Mutex<Tail>) -> Vec<u8> {
    std::mem::take(&mut *tail.lock().unwrap_or_else(PoisonError::into_inner)).finish()
}

/// The last `max` bytes of `text`, starting on a character boundary.
pub fn tail_of(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_command_runs_in_a_clean_environment() {
        let dir = tempfile::tempdir().unwrap();
        // The test process has variables that must not reach a check.
        std::env::set_var("MOLT_BENCH_SECRET_PROBE", "leaked");
        std::env::set_var("PYTHONPATH", "/somewhere");
        let ran = shell(
            dir.path(),
            "echo \"[$MOLT_BENCH_SECRET_PROBE$PYTHONPATH]\" \"$CI\" \"$PWD\"; echo oops >&2; exit 3",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!((ran.exit_code, ran.timed_out, ran.success()), (Some(3), false, false));
        let pwd = dir.path().canonicalize().unwrap();
        assert_eq!(ran.output, format!("[] 1 {}\noops\n", pwd.display()));
        assert_eq!(ran.ending(), "exit code 3");

        // Each command has an empty home of its own, which is gone after it.
        let home = "test -z \"$(ls -A \"$HOME\")\" && touch \"$HOME/mark\" && echo \"$HOME|$CARGO_HOME|$GOCACHE|$PYTHONNOUSERSITE\"";
        let first = shell(dir.path(), home, Duration::from_secs(10)).await.unwrap();
        let second = shell(dir.path(), home, Duration::from_secs(10)).await.unwrap();
        assert!(first.success() && second.success(), "{first:?} {second:?}");
        let parts: Vec<&str> = first.output.trim().split('|').collect();
        assert_eq!(parts[1], format!("{}/.cargo", parts[0]));
        assert_eq!(parts[3], "1");
        assert_ne!(first.output, second.output);
        assert!(!Path::new(parts[0]).exists());
    }

    #[tokio::test]
    async fn a_command_and_what_it_started_are_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let command = format!("(sleep 2; touch {}) & sleep 30", marker.display());
        let ran = shell(dir.path(), &command, Duration::from_millis(300)).await.unwrap();
        assert!(ran.timed_out && !ran.success(), "{ran:?}");
        assert!(ran.duration < Duration::from_secs(5));
        assert!(ran.ending().starts_with("timed out"));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!marker.exists(), "the background job was killed with the group");
    }

    #[tokio::test]
    async fn long_output_keeps_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let ran = shell(dir.path(), "seq 1 200000; echo last", Duration::from_secs(20)).await.unwrap();
        assert!(ran.success());
        assert!(ran.output.starts_with("[... "), "{}", &ran.output[..40]);
        assert!(ran.output.ends_with("200000\nlast\n"));
        assert!(ran.output.len() < KEEP + 100);
    }

    #[test]
    fn tails_start_on_a_character() {
        assert_eq!(tail_of("héllo", 4), "llo");
        assert_eq!(tail_of("abc", 10), "abc");
    }
}

//! Fetching a pack from a git repository: `molt ext add URL[#REF]`.
//!
//! Nothing of the repository runs, and neither the user's nor the system's
//! git configuration is read. Git runs with an environment emptied but for
//! what reaching a server takes (`PATH`, `HOME` for ssh keys and `.netrc`,
//! the proxy and CA variables, the ssh agent), the settings that would run a
//! program overridden on the command line (hooks, fsmonitor, credential
//! helpers, askpass), only the URL's own protocol allowed (`ext::` and the
//! other `TRANSPORT::` forms never are, nor a redirect to another protocol),
//! every object checked as it arrives (`transfer.fsckObjects`), and no
//! submodule fetched. It fetches one commit into a new bare repository made
//! without templates, and nothing is checked out: the commit's tree is listed
//! and its blobs written out by this code, so no attribute, filter, smudge
//! or LFS step applies. A symlink or a submodule in the tree refuses it. The
//! fetch has a deadline and a size limit; the files then go through the same
//! checks as a directory's.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};

use super::{plain, Tally, MAX_BYTES, MAX_DEPTH};

/// The protocols a pack is fetched over: encrypted ones that reach another machine.
pub const SCHEMES: &[&str] = &["https", "ssh"];
/// With `file`, for tests, which have no server to reach.
#[cfg(test)]
pub(super) const SCHEMES_AND_FILE: &[&str] = &["https", "ssh", "file"];

/// How long fetching a pack may take, its git processes together.
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
/// The most bytes the fetched repository may hold: one commit's objects, compressed.
const MAX_FETCHED: u64 = 2 * MAX_BYTES;
/// The longest URL and reference taken.
const MAX_URL: usize = 2048;
const MAX_REV: usize = 256;
/// Bytes of git's error output kept for a message.
const MAX_STDERR: usize = 4096;
/// The ref the fetched commit is kept under in the scratch repository.
const FETCHED: &str = "refs/molt/fetched";

/// Variables git runs with, besides those of [`PASSED`] that are set.
const ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_ATTR_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_NO_LAZY_FETCH", "1"),
    ("GIT_NO_REPLACE_OBJECTS", "1"),
    ("GIT_PROTOCOL_FROM_USER", "0"),
    // ssh never asks: no password, passphrase or unknown host key prompt.
    ("GIT_SSH_COMMAND", "ssh -o BatchMode=yes"),
    ("GIT_SSH_VARIANT", "ssh"),
    ("GIT_ADVICE", "0"),
    ("LC_ALL", "C"),
];

/// Variables of the user's that git is given: reaching the server, through
/// their proxy, with their CA bundle, ssh keys and agent.
const PASSED: &[&str] = &[
    "PATH",
    "HOME",
    "SSH_AUTH_SOCK",
    "https_proxy",
    "HTTPS_PROXY",
    "http_proxy",
    "HTTP_PROXY",
    "all_proxy",
    "ALL_PROXY",
    "no_proxy",
    "NO_PROXY",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
    "GIT_PROXY_SSL_CAINFO",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
];

/// Settings on the command line, which win over any other.
const CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.attributesFile=/dev/null",
    "core.askPass=",
    "credential.helper=",
    "protocol.allow=never",
    "protocol.version=2",
    "transfer.fsckObjects=true",
    "fetch.recurseSubmodules=false",
    "submodule.recurse=false",
    "fetch.writeCommitGraph=false",
    "gc.auto=0",
    "maintenance.auto=false",
    "init.templateDir=",
    "advice.detachedHead=false",
];

/// A pack's git repository and the reference to fetch from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSource {
    pub url: String,
    /// A branch, a tag or a commit id; `None` for the remote's `HEAD`.
    pub rev: Option<String>,
    /// The protocol git is allowed: `https`, `ssh` (also for `user@host:path`) or, in tests, `file`.
    scheme: &'static str,
}

impl GitSource {
    /// `arg` as a git source, when it is not a path that exists and reads
    /// as a URL; `None` when it is a path.
    pub fn from_arg(arg: &Path) -> anyhow::Result<Option<Self>> {
        if std::fs::symlink_metadata(arg).is_ok() {
            return Ok(None);
        }
        match arg.to_str() {
            Some(text) if looks_like_url(text) => Self::parse(text).map(Some),
            _ => Ok(None),
        }
    }

    /// `URL` or `URL#REF`, over the protocols of [`SCHEMES`].
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Self::parse_with(text, SCHEMES)
    }

    pub(super) fn parse_with(text: &str, schemes: &[&'static str]) -> anyhow::Result<Self> {
        let (url, rev) = match text.rsplit_once('#') {
            Some((url, rev)) => (url, Some(rev.to_owned())),
            None => (text, None),
        };
        if let Some(rev) = &rev {
            ensure!(valid_rev(rev), "{rev:?} after # is not a branch, a tag or a commit id");
        }
        Self::checked(url, rev, schemes)
    }

    /// A recorded source, checked again before it is fetched.
    pub(super) fn checked(url: &str, rev: Option<String>, schemes: &[&'static str]) -> anyhow::Result<Self> {
        if let Some((transport, _)) = url.split_once("::") {
            ensure!(
                transport.contains('/') || transport.contains(':'),
                "{url} uses git's `{transport}::` transport syntax, which runs a program to reach the \
                 repository; Molt fetches packs over https:// and ssh:// only"
            );
        }
        ensure!(
            !url.is_empty() && url.len() <= MAX_URL && !url.chars().any(|c| c.is_control() || c.is_whitespace()),
            "the git URL must be 1 to {MAX_URL} characters without spaces"
        );
        let scheme = if let Some((scheme, rest)) = url.split_once("://") {
            let scheme = scheme.to_ascii_lowercase();
            let Some(allowed) = schemes.iter().find(|s| **s == scheme) else {
                bail!(
                    "{url}: {}",
                    match scheme.as_str() {
                        "http" | "git" => "it is not encrypted, so anyone on the way could change it; Molt fetches \
                                           packs over https:// and ssh:// only"
                            .to_owned(),
                        "file" => "give the repository's directory instead: molt ext add DIR copies its files, \
                                   without its .git"
                            .to_owned(),
                        other => format!("Molt fetches packs over https:// and ssh:// only, not {other}://"),
                    }
                )
            };
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            if *allowed == "file" {
                ensure!(authority.is_empty() && !path.is_empty(), "{url}: a file:// URL names an absolute path");
            } else {
                let (user, host) = match authority.rsplit_once('@') {
                    Some((user, host)) => (Some(user), host),
                    None => (None, authority),
                };
                ensure!(
                    *allowed != "https" || user.is_none(),
                    "{url} has a user or a token in it, which would be recorded in extensions.toml: give the URL \
                     without it, and a token in ~/.netrc"
                );
                ensure!(
                    user.is_none_or(|u| !u.is_empty() && !u.starts_with('-') && !u.contains(':')),
                    "{url}: the user is not one ssh takes"
                );
                ensure!(valid_host(host), "{url}: {host:?} is not a host name");
                ensure!(!path.is_empty(), "{url} names no repository");
            }
            *allowed
        } else if let Some((user, host)) = scp_like(url) {
            ensure!(schemes.contains(&"ssh"), "{url} is reached over ssh, which is not allowed here");
            ensure!(!user.is_empty() && !user.starts_with('-'), "{url}: the user is not one ssh takes");
            ensure!(valid_host(host), "{url}: {host:?} is not a host name");
            "ssh"
        } else {
            bail!("{url} is neither a directory, an archive nor a git URL (https://… or ssh://…)")
        };
        Ok(Self { url: url.to_owned(), rev, scheme })
    }

    /// `URL#REF` as given.
    pub fn shown(&self) -> String {
        match &self.rev {
            Some(rev) => format!("{}#{rev}", self.url),
            None => self.url.clone(),
        }
    }

    /// What `self.rev` names, for messages.
    pub fn rev_shown(&self) -> &str {
        self.rev.as_deref().unwrap_or("HEAD")
    }

    /// Whether the reference is a whole commit id, which cannot move.
    pub fn pinned(&self) -> bool {
        self.rev.as_deref().is_some_and(is_commit_id)
    }
}

/// Whether `text` is meant as a git URL rather than a path.
fn looks_like_url(text: &str) -> bool {
    text.contains("://") || text.contains("::") || scp_like(text.split('#').next().unwrap_or(text)).is_some()
}

/// `user@host:path`, git's short form of an ssh URL: the user and the host.
fn scp_like(text: &str) -> Option<(&str, &str)> {
    let (before, path) = text.split_once(':')?;
    let (user, host) = before.split_once('@')?;
    (!before.contains('/') && !path.is_empty()).then_some((user, host))
}

fn valid_host(host: &str) -> bool {
    let name = host.rsplit_once(':').map_or(host, |(name, port)| {
        if port.bytes().all(|b| b.is_ascii_digit()) && !port.is_empty() {
            name
        } else {
            host
        }
    });
    let name = name.strip_prefix('[').and_then(|n| n.strip_suffix(']')).unwrap_or(name);
    !name.is_empty()
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-.:_".contains(&b))
}

/// A branch, a tag or a commit id: a ref name that is not an option, a
/// range or a revision expression.
fn valid_rev(rev: &str) -> bool {
    !rev.is_empty()
        && rev.len() <= MAX_REV
        && rev.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))
        && !rev.starts_with(['-', '/', '.'])
        && !rev.ends_with(['/', '.'])
        && !rev.ends_with(".lock")
        && !rev.contains("..")
        && !rev.contains("//")
        && !rev.contains("/.")
        && rev != "HEAD"
}

fn is_commit_id(rev: &str) -> bool {
    matches!(rev.len(), 40 | 64) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Fetch `source`'s commit into a new repository under `work` and write its
/// files into the empty `files`. Returns the commit's id.
pub(super) fn fetch(source: &GitSource, work: &Path, files: &Path) -> anyhow::Result<String> {
    let deadline = Instant::now() + FETCH_TIMEOUT;
    let template = work.join("template");
    let repo = work.join("repo.git");
    std::fs::create_dir(&template)?;
    let mut init = git(work, source.scheme);
    init.arg("init").arg("--bare").arg("-q").arg(format!("--template={}", template.display())).arg(&repo);
    run(init, "init", None, deadline, None)?;

    let rev = source.rev.as_deref().unwrap_or("HEAD");
    let mut fetch = git(work, source.scheme);
    fetch.arg("--git-dir").arg(&repo).args([
        "fetch",
        "--quiet",
        "--no-tags",
        "--depth=1",
        "--no-recurse-submodules",
        "--no-write-fetch-head",
        "--no-auto-gc",
        "--end-of-options",
        &source.url,
        &format!("+{rev}:{FETCHED}"),
    ]);
    run(fetch, "fetch", None, deadline, Some(&repo)).with_context(|| format!("fetching {}", source.shown()))?;

    let mut parse = git(work, source.scheme);
    parse.arg("--git-dir").arg(&repo).args([
        "rev-parse",
        "--verify",
        "--end-of-options",
        &format!("{FETCHED}^{{commit}}"),
    ]);
    let commit = String::from_utf8(run(parse, "rev-parse", None, deadline, None)?)?.trim().to_owned();
    ensure!(is_commit_id(&commit), "git named the fetched commit {commit:?}");
    if let Some(rev) = source.rev.as_deref().filter(|r| is_commit_id(r)) {
        ensure!(commit.eq_ignore_ascii_case(rev), "{} fetched commit {commit}, not {rev}", source.url);
    }

    let mut list = git(work, source.scheme);
    list.arg("--git-dir").arg(&repo).args(["ls-tree", "-r", "-l", "-z", "--full-tree", "--end-of-options", &commit]);
    let listing = run(list, "ls-tree", None, deadline, None)?;
    let entries = tree_entries(&listing)?;

    let mut tally = Tally::default();
    let mut dirs = std::collections::BTreeSet::new();
    for entry in &entries {
        let mut at = PathBuf::new();
        let parts: Vec<&str> = entry.path.split('/').collect();
        ensure!(parts.len() <= MAX_DEPTH, "{} is deeper than {MAX_DEPTH} directories", entry.path);
        for part in &parts[..parts.len() - 1] {
            at.push(part);
            if dirs.insert(at.clone()) {
                tally.add(0)?;
            }
        }
        tally.add(entry.size)?;
    }
    let mut input = Vec::new();
    for entry in &entries {
        input.extend_from_slice(entry.oid.as_bytes());
        input.push(b'\n');
    }
    let mut cat = git(work, source.scheme);
    cat.arg("--git-dir").arg(&repo).args(["cat-file", "--batch"]);
    let blobs = run(cat, "cat-file", Some(input), deadline, None)?;
    let mut rest = blobs.as_slice();
    for dir in &dirs {
        std::fs::create_dir(files.join(dir))?;
    }
    for entry in &entries {
        let line_end = rest.iter().position(|b| *b == b'\n').context("git's blob answer ended early")?;
        let header = std::str::from_utf8(&rest[..line_end])?;
        let size: usize = match header.split(' ').collect::<Vec<_>>().as_slice() {
            [oid, "blob", size] if *oid == entry.oid => size.parse()?,
            _ => bail!("git answered {header:?} for {}", entry.path),
        };
        ensure!(size as u64 == entry.size, "{} is not as long as its tree says", entry.path);
        let body = rest.get(line_end + 1..line_end + 1 + size).context("git's blob answer ended early")?;
        rest = rest.get(line_end + 2 + size..).context("git's blob answer ended early")?;
        let to = files.join(&entry.path);
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&to)
            .with_context(|| format!("{} is in the tree twice", entry.path))?;
        out.write_all(body)?;
        let mode = if entry.exec { 0o755 } else { 0o644 };
        std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(commit)
}

/// A file of the fetched tree.
struct TreeEntry {
    path: String,
    oid: String,
    size: u64,
    exec: bool,
}

/// The files of `git ls-tree -r -l -z`, refusing a symlink, a submodule and
/// any path that is not plain.
fn tree_entries(listing: &[u8]) -> anyhow::Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let record = std::str::from_utf8(record).context("a path in the repository is not UTF-8")?;
        let (meta, path) = record.split_once('\t').context("git listed the tree in a form it does not read")?;
        let fields: Vec<&str> = meta.split_whitespace().collect();
        let [mode, kind, oid, size] = fields.as_slice() else { bail!("git listed {path} as {meta:?}") };
        ensure!(plain(path) && !path.contains('\\'), "{path:?} in the repository is not a plain relative path");
        ensure!(
            !path.split('/').any(|p| p.eq_ignore_ascii_case(".git")),
            "{path} in the repository is a .git, which a pack may not hold"
        );
        match (*mode, *kind) {
            ("100644" | "100755", "blob") => entries.push(TreeEntry {
                path: path.to_owned(),
                oid: (*oid).to_owned(),
                size: size.parse().with_context(|| format!("git listed {path} with size {size:?}"))?,
                exec: *mode == "100755",
            }),
            ("120000", _) => bail!("{path} is a symlink, which a pack may not hold; nothing was installed"),
            ("160000", _) => bail!(
                "{path} is a submodule, which a pack may not hold (Molt fetches no submodule); nothing was installed"
            ),
            _ => bail!("{path} is neither a file nor a directory (mode {mode})"),
        }
    }
    Ok(entries)
}

/// Git in `cwd`, with the environment and settings above and only `scheme`
/// allowed.
fn git(cwd: &Path, scheme: &str) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear();
    for name in PASSED {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    cmd.envs(ENV.iter().copied()).env("GIT_ALLOW_PROTOCOL", scheme);
    for setting in CONFIG {
        cmd.arg("-c").arg(setting);
    }
    cmd.arg("-c").arg(format!("protocol.{scheme}.allow=always"));
    cmd.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

/// Run `cmd` with `input` on stdin until it ends or `deadline`, keeping
/// what it prints, up to a bound; while `watch` holds more than
/// [`MAX_FETCHED`] bytes it is stopped.
fn run(
    mut cmd: Command,
    what: &str,
    input: Option<Vec<u8>>,
    deadline: Instant,
    watch: Option<&Path>,
) -> anyhow::Result<Vec<u8>> {
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().context("git could not be run; install git to add a pack from a repository")?;
    let feeder = input.zip(child.stdin.take()).map(|(input, mut stdin)| {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        })
    });
    let stdout = drain(child.stdout.take(), MAX_FETCHED as usize);
    let stderr = drain(child.stderr.take(), MAX_STDERR);
    let stopped = wait(&mut child, deadline, watch);
    if let Some(feeder) = feeder {
        let _ = feeder.join();
    }
    let (out, cut) = stdout.join().unwrap_or_default();
    let (err, _) = stderr.join().unwrap_or_default();
    let status = stopped?;
    ensure!(!cut, "git {what} printed more than {} MiB", MAX_FETCHED / 1024 / 1024);
    if !status.success() {
        let err = String::from_utf8_lossy(&err);
        let err = err.trim();
        bail!("git {what} failed{}{err}", if err.is_empty() { "" } else { ": " });
    }
    Ok(out)
}

/// Wait for `child`, killed at `deadline` or once `watch` grows past the limit.
fn wait(child: &mut Child, deadline: Instant, watch: Option<&Path>) -> anyhow::Result<std::process::ExitStatus> {
    let mut next_look = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let now = Instant::now();
        let stop = if now >= deadline {
            Some(format!("git took longer than {} s and was stopped", FETCH_TIMEOUT.as_secs()))
        } else if let Some(dir) = watch.filter(|_| now >= next_look) {
            next_look = now + Duration::from_millis(100);
            (size_of(dir) > MAX_FETCHED)
                .then(|| format!("the repository holds more than {} MiB, so it was not fetched", MAX_FETCHED >> 20))
        } else {
            None
        };
        if let Some(why) = stop {
            let _ = child.kill();
            let _ = child.wait();
            bail!(why);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Bytes of the files under `dir`, not following symlinks.
fn size_of(dir: &Path) -> u64 {
    let mut total = 0;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            match entry.metadata() {
                Ok(m) if m.is_dir() => todo.push(entry.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// Read a pipe to its end on a thread of its own, keeping `limit` bytes; true when it had more.
fn drain(pipe: Option<impl Read + Send + 'static>, limit: usize) -> JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let Some(mut pipe) = pipe else { return (kept, false) };
        let _ = (&mut pipe).take(limit as u64).read_to_end(&mut kept);
        let more = std::io::copy(&mut pipe, &mut std::io::sink()).unwrap_or(0) > 0;
        (kept, more)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_and_ssh_urls_are_fetched_and_never_through_a_transport_program() {
        for (url, says) in [
            ("ext::sh -c touch% /tmp/pwned", "transport syntax"),
            ("fd::17", "transport syntax"),
            ("https::example.com/repo", "transport syntax"),
            ("http://example.com/pack.git", "not encrypted"),
            ("git://example.com/pack.git", "not encrypted"),
            ("file:///tmp/pack", "give the repository's directory"),
            ("ftp://example.com/pack.git", "not ftp://"),
            ("https://user:token@example.com/pack.git", "a user or a token"),
            ("ssh://-oProxyCommand=touch%20x/pack", "not a host name"),
            ("-oProxyCommand=x@host:pack", "the user is not one"),
            ("https://example.com/pack.git#--upload-pack=touch", "is not a branch"),
            ("https://example.com/pack.git#main..evil", "is not a branch"),
            ("https://example.com/a b.git", "without spaces"),
            ("https://example.com", "names no repository"),
        ] {
            let err = format!("{:#}", GitSource::parse(url).unwrap_err());
            assert!(err.contains(says), "{url}: {err}");
        }
        let s = GitSource::parse("https://example.com/team/tracker.git#v1.2").unwrap();
        assert_eq!(
            (s.url.as_str(), s.rev.as_deref(), s.scheme),
            ("https://example.com/team/tracker.git", Some("v1.2"), "https")
        );
        assert_eq!(GitSource::parse("git@example.com:team/tracker.git").unwrap().scheme, "ssh");
        assert_eq!(GitSource::parse("ssh://git@example.com:2222/team/tracker").unwrap().scheme, "ssh");
        assert!(GitSource::parse(&format!("https://example.com/x#{}", "a".repeat(40))).unwrap().pinned());
        assert_eq!(GitSource::parse_with("file:///srv/pack", SCHEMES_AND_FILE).unwrap().scheme, "file");
        // A path is a path, whatever it holds; a URL-like name that does not exist is a URL.
        assert_eq!(GitSource::from_arg(Path::new("./pack-dir")).unwrap(), None);
        assert!(GitSource::from_arg(Path::new("ext::sh -c x")).is_err());
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join("ext::odd");
        std::fs::create_dir(&odd).unwrap();
        assert_eq!(GitSource::from_arg(&odd).unwrap(), None);
    }
}

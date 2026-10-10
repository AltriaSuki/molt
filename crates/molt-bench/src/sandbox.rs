//! Running the agent's commands in Molt's sandbox. The sandbox shows
//! commands only the system directories, so the toolchains the tasks need
//! (a rustup install, a Node in /opt) are mounted into it read-only. The
//! tasks' hidden tests stay out of it.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};
use molt_api::fs::ForkResponse;
use molt_api::shell::RunResponse;
use molt_tools::{ExecutionPolicy, Fs, Roots, SandboxPolicy, Shell};
use serde_json::json;

use crate::task::Language;

/// What the sandbox shows every command, so tools there need no mount.
const VISIBLE: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64"];
/// Directories never mounted whole: the sandbox refuses most of them, and
/// the rest hold far more than one toolchain.
const UNMOUNTABLE: &[&str] = &["/", "/proc", "/dev", "/sys", "/etc", "/run", "/tmp", "/home", "/root", "/var", "/opt"];

/// The commands a language's tasks run, the first of them in every check.
fn tools(language: Language) -> &'static [&'static str] {
    match language {
        Language::Python => &["python3"],
        Language::Javascript => &["node", "npm", "npx"],
        Language::Rust => &["cargo", "rustc"],
        Language::Go => &["go", "gofmt"],
    }
}

/// A command that shows a language's toolchain works.
fn probe(language: Language) -> &'static str {
    match language {
        Language::Python => "python3 --version",
        Language::Javascript => "node --version",
        Language::Rust => "cargo --version",
        Language::Go => "go version",
    }
}

/// What the tasks' toolchains need from the sandbox.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Toolchains {
    pub languages: Vec<Language>,
    /// Directories to mount read-only.
    pub read_only: Vec<PathBuf>,
    /// Variables commands need, with the values every run sets.
    pub env: Vec<(String, String)>,
}

impl Toolchains {
    /// Find the toolchains of `languages` on `path` (a `PATH` value).
    /// `rustup_home` is where rustup keeps its toolchains, if it is
    /// installed. No directory in `keep_out`, nor any holding one, is
    /// mounted: the tasks with their hidden tests, the runs' workspaces,
    /// the user's home.
    pub fn find(
        languages: &[Language],
        path: &OsStr,
        rustup_home: Option<&Path>,
        keep_out: &[&Path],
    ) -> anyhow::Result<Self> {
        let keep_out: Vec<PathBuf> = keep_out
            .iter()
            .map(|p| p.canonicalize().or_else(|_| std::path::absolute(p)).unwrap_or_else(|_| p.to_path_buf()))
            .collect();
        let mut languages = languages.to_vec();
        languages.sort();
        languages.dedup();
        let mut dirs = Vec::new();
        let mut env = Vec::new();
        for &language in &languages {
            for (i, tool) in tools(language).iter().enumerate() {
                let Some(found) = find_in_path(tool, path) else {
                    ensure!(i > 0, "the {language} tasks need {tool}, which is not on PATH");
                    continue;
                };
                let real = found.canonicalize().with_context(|| format!("resolving {}", found.display()))?;
                for exe in [&found, &real] {
                    dirs.push(install_dir(exe, &keep_out)?);
                }
                // A rustup proxy runs the toolchain rustup keeps in its home.
                if rustup_proxy(&found, &real) {
                    let home = rustup_home.context("cargo is rustup's, but rustup has no home directory")?;
                    let home = home.canonicalize().with_context(|| format!("rustup's home {}", home.display()))?;
                    ensure!(
                        !holds_any(&home, &keep_out),
                        "rustup's home {} holds the benchmark's files",
                        home.display()
                    );
                    dirs.push(home.clone());
                    let var = ("RUSTUP_HOME".to_owned(), home.display().to_string());
                    if !env.contains(&var) {
                        env.push(var);
                    }
                }
            }
        }
        let mut read_only: Vec<PathBuf> = Vec::new();
        dirs.retain(|d| !VISIBLE.iter().any(|v| d.starts_with(v)));
        dirs.sort();
        for dir in dirs {
            if !read_only.iter().any(|kept| dir.starts_with(kept)) {
                read_only.push(dir);
            }
        }
        ensure!(read_only.len() <= 32, "the toolchains are in more than 32 directories");
        Ok(Toolchains { languages, read_only, env })
    }

    /// The sandbox policy for the runs' commands: Molt's defaults, with the
    /// toolchains mounted and their variables passed.
    pub fn policy(&self) -> SandboxPolicy {
        SandboxPolicy {
            read_only: self.read_only.clone(),
            environment: self.env.iter().map(|(name, _)| name.clone()).collect(),
            ..SandboxPolicy::default()
        }
    }

    /// Check that the sandbox works here and that each toolchain runs in it,
    /// as the runs' commands will, from a fork under `scratch`.
    pub async fn check(&self, scratch: &Path) -> anyhow::Result<()> {
        let tmp = tempfile::Builder::new()
            .prefix("molt-bench-sandbox-")
            .tempdir_in(scratch)
            .with_context(|| format!("making a directory in {}", scratch.display()))?;
        let root = tmp.path().join("project");
        std::fs::create_dir(&root)?;
        std::fs::write(root.join("README.md"), "A sandbox check.\n")?;
        let roots = Roots { root: root.clone(), scratch: tmp.path().join("scratch") };
        let shell = Shell::with_policy(roots.clone(), ExecutionPolicy::Configured(self.policy())).map_err(|e| {
            anyhow::anyhow!(
                "the agent's commands run in Molt's sandbox, which does not work here: {e:#}. Install bubblewrap \
                 0.9 or newer, or pass --no-sandbox to run them unconfined"
            )
        })?;
        let files = Fs::new(roots)?;
        let fork = files.handle("fork", json!({ "workspace": root })).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let fork: ForkResponse = serde_json::from_value(fork)?;
        // The sandbox passes variables from this process, which may not
        // have them; the runs get them from the benchmark.
        let assign: String = self.env.iter().map(|(name, value)| format!("{name}={} ", quote(value))).collect();
        for &language in &self.languages {
            let command = format!("{assign}{}", probe(language));
            let ran = shell
                .handle("run", json!({ "workspace": fork.fork, "command": command, "timeout_ms": 60_000 }))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let ran: RunResponse = serde_json::from_value(ran)?;
            if !ran.success() {
                let output = format!("{}{}", ran.stdout, ran.stderr);
                bail!(
                    "`{}` fails in Molt's sandbox ({}): {}. Pass --no-sandbox to run the agent's commands unconfined",
                    probe(language),
                    match ran.exit_code {
                        Some(code) => format!("exit code {code}"),
                        None => "killed".to_owned(),
                    },
                    output.trim()
                );
            }
        }
        Ok(())
    }
}

/// Write `policy` to `path` for `molt do --sandbox-policy`, which reads only
/// a file of this user's that no one else can change.
pub fn write_policy(path: &Path, policy: &SandboxPolicy) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer_pretty(&mut file, policy)?;
    Ok(())
}

/// The directory a tool is installed in, to mount: the prefix above its
/// `bin` directory, or its own directory when that prefix cannot be
/// mounted or holds something in `keep_out`.
fn install_dir(exe: &Path, keep_out: &[PathBuf]) -> anyhow::Result<PathBuf> {
    let dir = exe.parent().context("a tool with no directory")?.canonicalize()?;
    if VISIBLE.iter().any(|v| dir.starts_with(v)) {
        return Ok(dir);
    }
    let prefix = (dir.file_name() == Some(OsStr::new("bin"))).then(|| dir.parent()).flatten();
    for candidate in prefix.into_iter().chain([dir.as_path()]) {
        let unmountable = UNMOUNTABLE.iter().any(|p| candidate == Path::new(p));
        if !unmountable && !holds_any(candidate, keep_out) {
            return Ok(candidate.to_owned());
        }
    }
    bail!(
        "{} is in {}, which the sandbox cannot show without the benchmark's files or a whole system directory; \
         install it elsewhere, or pass --no-sandbox",
        exe.display(),
        dir.display()
    )
}

/// Whether `found` is one of rustup's proxies: a link to rustup, symbolic
/// or hard.
fn rustup_proxy(found: &Path, real: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if real.file_name().is_some_and(|n| n.to_string_lossy().starts_with("rustup")) {
        return true;
    }
    let same = |a: &std::fs::Metadata, b: &std::fs::Metadata| a.dev() == b.dev() && a.ino() == b.ino();
    match (std::fs::metadata(found), found.parent().map(|d| std::fs::metadata(d.join("rustup")))) {
        (Ok(tool), Some(Ok(rustup))) => same(&tool, &rustup),
        _ => false,
    }
}

/// Whether `dir` is, or holds, one of `paths`.
fn holds_any(dir: &Path, paths: &[PathBuf]) -> bool {
    paths.iter().any(|p| p.starts_with(dir))
}

/// The first `tool` on `path` that is an executable file.
fn find_in_path(tool: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(tool))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// `s` as one shell word.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn exe(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn paths(dirs: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn a_toolchain_is_mounted_from_the_prefix_above_its_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        exe(&root.join("node22/bin/node"));
        exe(&root.join("node22/lib/node_modules/npm/bin/npm-cli.js"));
        symlink("../lib/node_modules/npm/bin/npm-cli.js", root.join("node22/bin/npm")).unwrap();
        let t = Toolchains::find(&[Language::Javascript], &paths(&[&root.join("node22/bin")]), None, &[]).unwrap();
        assert_eq!(t.read_only, [root.join("node22")]);
        assert!(t.env.is_empty());
    }

    #[test]
    fn rustup_proxies_bring_rustups_home_and_its_variable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        exe(&root.join("cargo/bin/rustup"));
        symlink("rustup", root.join("cargo/bin/cargo")).unwrap();
        symlink("rustup", root.join("cargo/bin/rustc")).unwrap();
        std::fs::create_dir(root.join("rustup")).unwrap();
        let path = paths(&[&root.join("cargo/bin")]);
        let t = Toolchains::find(&[Language::Rust], &path, Some(&root.join("rustup")), &[]).unwrap();
        assert_eq!(t.read_only, [root.join("cargo"), root.join("rustup")]);
        assert_eq!(t.env, [("RUSTUP_HOME".to_owned(), root.join("rustup").display().to_string())]);
        assert_eq!(t.policy().environment, ["RUSTUP_HOME"]);
        assert!(Toolchains::find(&[Language::Rust], &path, None, &[]).is_err(), "no rustup home");
        // rustup makes its proxies hard links where it can.
        for tool in ["cargo", "rustc"] {
            std::fs::remove_file(root.join("cargo/bin").join(tool)).unwrap();
            std::fs::hard_link(root.join("cargo/bin/rustup"), root.join("cargo/bin").join(tool)).unwrap();
        }
        let hard = Toolchains::find(&[Language::Rust], &path, Some(&root.join("rustup")), &[]).unwrap();
        assert_eq!(hard, t);
    }

    #[test]
    fn tools_in_the_system_directories_need_no_mount() {
        let path = paths(&[Path::new("/usr/bin"), Path::new("/bin")]);
        let t = Toolchains::find(&[Language::Python], &path, None, &[]).unwrap();
        assert!(t.read_only.is_empty());
    }

    #[test]
    fn a_missing_toolchain_is_named() {
        let tmp = tempfile::tempdir().unwrap();
        let err = Toolchains::find(&[Language::Go], &paths(&[tmp.path()]), None, &[]).unwrap_err();
        assert!(err.to_string().contains("go tasks need go"), "{err}");
    }

    #[test]
    fn nothing_holding_the_benchmarks_files_is_mounted() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap().join("home");
        exe(&home.join("bin/node"));
        let path = paths(&[&home.join("bin")]);
        // The prefix above bin is the home with the tasks in it, so only bin is mounted.
        let tasks = home.join("molt/bench/tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        let t = Toolchains::find(&[Language::Javascript], &path, None, &[&tasks]).unwrap();
        assert_eq!(t.read_only, [home.join("bin")]);
        // A tool beside the tasks cannot be mounted at all.
        exe(&tasks.join("node"));
        let err = Toolchains::find(&[Language::Javascript], &paths(&[&tasks]), None, &[&tasks]).unwrap_err();
        assert!(err.to_string().contains("--no-sandbox"), "{err}");
    }

    #[test]
    fn the_policy_file_is_private() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("logs/sandbox-policy.json");
        let policy = SandboxPolicy { read_only: vec![tmp.path().to_owned()], ..SandboxPolicy::default() };
        write_policy(&path, &policy).unwrap();
        write_policy(&path, &policy).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let read: SandboxPolicy = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(read.read_only, policy.read_only);
    }

    #[test]
    fn quoting_survives_quotes() {
        assert_eq!(quote("a'b"), r"'a'\''b'");
    }
}

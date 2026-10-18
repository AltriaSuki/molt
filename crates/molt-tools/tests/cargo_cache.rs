//! `--deps-cache cargo`: a crate with registry and git dependencies builds
//! and tests in the sandbox, offline, from a host Cargo cache that stays
//! read-only. The cache is made here, by the host's cargo fetching from a
//! registry served on localhost and a local git repository, so the test
//! needs no network.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use molt_api::fs::ForkResponse;
use molt_api::shell::RunResponse;
use molt_tools::{ExecutionPolicy, Fs, Roots, SandboxPolicy, Shell};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The host's cargo, unconfined, with `home` as its Cargo home and nothing
/// of the developer's or the proxy's settings.
fn cargo(dir: &Path, home: &Path, args: &[&str]) {
    let mut command = Command::new("cargo");
    command.args(args).current_dir(dir).env("CARGO_HOME", home).env("CARGO_TARGET_DIR", dir.join("target"));
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy", "CARGO_HTTP_PROXY"]
    {
        command.env_remove(name);
    }
    command.env("NO_PROXY", "127.0.0.1").env("no_proxy", "127.0.0.1");
    let out = command.output().unwrap();
    assert!(out.status.success(), "cargo {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A library crate `name` whose `value()` is `value`.
fn library(dir: &Path, name: &str, value: u32) {
    write(
        &dir.join("Cargo.toml"),
        &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\nlicense = \"MIT\"\ndescription = \"x\"\n"),
    );
    write(&dir.join("src/lib.rs"), &format!("pub fn value() -> u32 {{ {value} }}\n"));
}

/// Serve `files` over HTTP/1.1 from `listener` until the task is aborted.
fn serve(listener: tokio::net::TcpListener, files: HashMap<String, Vec<u8>>) -> tokio::task::JoinHandle<()> {
    let files = Arc::new(files);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let files = files.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let (status, body) = match files.get(path) {
                    Some(body) => ("200 OK", body.clone()),
                    None => ("404 Not Found", Vec::new()),
                };
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    })
}

/// A registry crate's `.crate` file, made by the host's cargo.
fn package(dir: &Path, name: &str, value: u32, home: &Path) -> Vec<u8> {
    library(dir, name, value);
    cargo(dir, home, &["package", "--offline", "--no-verify", "--allow-dirty", "--quiet"]);
    fs::read(dir.join(format!("target/package/{name}-0.1.0.crate"))).unwrap()
}

/// What `cargo` on PATH needs mounted: its directory and, for a rustup
/// proxy, rustup's home, with the variable that names it.
fn toolchain() -> (Vec<PathBuf>, String) {
    let path = std::env::var_os("PATH").unwrap();
    let found = std::env::split_paths(&path).map(|dir| dir.join("cargo")).find(|p| p.is_file()).expect("cargo on PATH");
    let dir = found.parent().unwrap().canonicalize().unwrap();
    let mut mounts = Vec::new();
    if !["/usr", "/bin"].iter().any(|system| dir.starts_with(system)) {
        mounts.push(dir);
    }
    let mut assign = String::new();
    if found.canonicalize().unwrap().file_name().is_some_and(|name| name == "rustup") {
        let home = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".rustup"))
            .canonicalize()
            .unwrap();
        assign = format!("RUSTUP_HOME='{}' ", home.display());
        mounts.push(home);
    }
    (mounts, assign)
}

/// Every path under `dir` with its size, to see that nothing changed.
fn snapshot(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let meta = fs::symlink_metadata(entry.path()).unwrap();
            if meta.is_dir() {
                stack.push(entry.path());
            }
            out.push((entry.path(), if meta.is_file() { meta.len() } else { 0 }));
        }
    }
    out.sort();
    out
}

// Several threads: the host's cargo blocks one while the registry is served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crate_with_registry_and_git_dependencies_tests_offline_from_the_read_only_host_cache() {
    let (toolchain, rustup) = toolchain();
    let probe = SandboxPolicy { read_only: toolchain.clone(), ..Default::default() };
    if let Err(error) = probe.probe() {
        assert!(std::env::var_os("MOLT_REQUIRE_SANDBOX_TESTS").is_none(), "required sandbox unavailable: {error:#}");
        eprintln!("namespace integration test unavailable in this executor: {error:#}");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let host = tmp_path.join("host-cargo");
    fs::create_dir(&host).unwrap();
    // Never to be seen by commands.
    write(&host.join("credentials.toml"), "[registry]\ntoken = \"secret\"\n");

    // A registry with two crates.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut files = HashMap::new();
    for (name, value) in [("alpha", 1), ("bravo", 2)] {
        let bytes = package(&tmp_path.join("crates").join(name), name, value, &host);
        let sum = hex::encode(Sha256::digest(&bytes));
        let entry = json!({"name": name, "vers": "0.1.0", "deps": [], "cksum": sum, "features": {}, "yanked": false});
        files.insert(format!("/{}/{}/{name}", &name[..2], &name[2..4]), format!("{entry}\n").into_bytes());
        files.insert(format!("/dl/{name}/0.1.0"), bytes);
    }
    files.insert(
        "/config.json".into(),
        format!("{{\"dl\":\"http://127.0.0.1:{port}/dl/{{crate}}/{{version}}\"}}").into_bytes(),
    );
    let server = serve(listener, files);

    // A git dependency in a local repository.
    let repo = tmp_path.join("charlie");
    library(&repo, "charlie", 3);
    for args in
        [&["init", "-q"][..], &["add", "."], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]]
    {
        assert!(Command::new("git").args(args).current_dir(&repo).status().unwrap().success());
    }

    let project = tmp_path.join("project");
    let deps = format!(
        "alpha = {{ version = \"0.1\", registry = \"fake\" }}\nbravo = {{ version = \"0.1\", registry = \"fake\" }}\n\
         charlie = {{ git = \"file://{}\" }}\n",
        repo.display()
    );
    write(
        &project.join("Cargo.toml"),
        &format!("[package]\nname = \"project\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{deps}"),
    );
    write(
        &project.join(".cargo/config.toml"),
        &format!("[registries.fake]\nindex = \"sparse+http://127.0.0.1:{port}/\"\n"),
    );
    write(&project.join(".gitignore"), "target\n");
    write(
        &project.join("src/lib.rs"),
        "pub fn sum() -> u32 { alpha::value() + bravo::value() + charlie::value() }\n\n\
         #[test]\nfn sums_the_dependencies() { assert_eq!(sum(), 6); }\n",
    );
    cargo(&project, &host, &["fetch", "--quiet"]);
    server.abort();
    fs::remove_dir_all(project.join("target")).ok();
    // One source the host never extracted: the sandbox extracts it from the `.crate`.
    let src = host.join("registry/src");
    let index = fs::read_dir(&src).unwrap().next().unwrap().unwrap().path();
    fs::remove_dir_all(index.join("bravo-0.1.0")).unwrap();
    let before = snapshot(&host);

    let roots = Roots { root: project.clone(), scratch: tmp_path.join("scratch") };
    let fork: ForkResponse = serde_json::from_value(
        Fs::new(roots.clone()).unwrap().handle("fork", json!({"workspace": project})).await.unwrap(),
    )
    .unwrap();
    let policy = SandboxPolicy { read_only: toolchain, cargo_cache: Some(host.clone()), ..Default::default() };
    let shell = Shell::with_policy(roots, ExecutionPolicy::Configured(policy)).unwrap();
    let run = |command: String| {
        let (shell, fork) = (&shell, fork.fork.clone());
        async move {
            let reply = shell.handle("run", json!({"workspace": fork, "command": command, "timeout_ms": 300_000}));
            serde_json::from_value::<RunResponse>(reply.await.unwrap()).unwrap()
        }
    };
    let tested = run(format!("{rustup}cargo test 2>&1")).await;
    assert!(tested.success(), "{tested:?}");
    assert!(tested.stdout.contains("sums_the_dependencies ... ok"), "{}", tested.stdout);
    assert!(!tested.offline_failure);

    // Only the caches are there, read-only; the private home is the command's own.
    let host_path = host.display();
    let seen = run(format!(
        "test \"$CARGO_NET_OFFLINE\" = true && test \"$CARGO_HOME\" != '{host_path}' && \
         test -d '{host_path}/registry' && test ! -e '{host_path}/credentials.toml' && \
         test ! -e \"$CARGO_HOME/credentials.toml\" && ! touch '{host_path}/registry/new' && touch \"$CARGO_HOME/new\""
    ))
    .await;
    assert!(seen.success(), "{seen:?}");
    assert_eq!(snapshot(&host), before, "the host cache was changed");

    // A dependency that is not in the cache fails, and says it is for want of network.
    let missing = run(format!(
        "printf 'gamma = {{ version = \"0.1\", registry = \"fake\" }}\\n' >> Cargo.toml && {rustup}cargo build 2>&1"
    ))
    .await;
    assert!(!missing.success() && missing.offline_failure, "{missing:?}");
}

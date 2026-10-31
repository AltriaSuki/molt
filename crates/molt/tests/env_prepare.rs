//! `molt env prepare` on a real project, in the real sandbox, from a fake
//! package index on this machine: nothing here reaches the network.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

/// What the index serves: content type and body, by path.
type Routes = Arc<Mutex<BTreeMap<String, (String, Vec<u8>)>>>;

/// A package index on 127.0.0.1 serving fixed pages and files, counting requests.
struct Index {
    origin: String,
    port: u16,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Index {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes: Routes = Default::default();
        let hits: Arc<Mutex<Vec<String>>> = Default::default();
        let (served, seen) = (routes.clone(), hits.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (served, seen) = (served.clone(), seen.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                            break;
                        }
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
                    seen.lock().unwrap().push(path.clone());
                    let answer = served.lock().unwrap().get(&path).cloned();
                    let (status, kind, body) = match answer {
                        Some((kind, body)) => ("200 OK", kind, body),
                        None => ("404 Not Found", "text/plain".to_owned(), Vec::new()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&body));
                });
            }
        });
        Self { origin: format!("http://127.0.0.1:{port}"), port, routes, hits }
    }

    fn serve(&self, path: &str, kind: &str, body: Vec<u8>) {
        self.routes.lock().unwrap().insert(path.to_owned(), (kind.to_owned(), body));
    }

    fn hits(&self, prefix: &str) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.starts_with(prefix)).count()
    }
}

/// The user's machine: its home, config, cache (the data dir), and a project.
struct Machine {
    tmp: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["home", "config/molt", "cache", "project", "dist"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        Self { tmp }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().canonicalize().unwrap().join(rel)
    }

    fn config(&self, text: &str) {
        std::fs::write(self.path("config/molt/config.toml"), text).unwrap();
    }

    /// `molt ARGS` as the user, with credentials in its environment that no command may see.
    fn molt(&self, args: &[&str]) -> (i32, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_molt"))
            .args(args)
            .arg("--workspace")
            .arg(self.path("project"))
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("ANTHROPIC_API_KEY", "sk-ant-gateway-secret")
            .env("MOLT_SOME_TOKEN", "molt-bus-secret")
            .env("GLM_API_KEY", "glm-endpoint-secret")
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .env_remove("ALL_PROXY")
            .output()
            .unwrap();
        let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        (out.status.code().unwrap_or(-1), said)
    }

    fn records(&self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for entry in walk(&self.path("cache")) {
            if entry.ends_with("deps/records.jsonl") {
                let text = std::fs::read_to_string(entry).unwrap();
                out.extend(text.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()));
            }
        }
        out
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Build the packages with Python: two wheels and a source distribution
/// whose in-tree build backend records what it could reach.
fn build_packages(machine: &Machine, port: u16) {
    let script = r#"
import base64, hashlib, io, os, sys, tarfile, zipfile
out, project, home, port = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]

def wheel(path, name, version, files):
    files = dict(files)
    info = f"{name}-{version}.dist-info"
    files[f"{info}/METADATA"] = f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n".encode()
    files[f"{info}/WHEEL"] = b"Wheel-Version: 1.0\nGenerator: molt-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n"
    record = []
    for p, b in files.items():
        h = base64.urlsafe_b64encode(hashlib.sha256(b).digest()).rstrip(b"=").decode()
        record.append(f"{p},sha256={h},{len(b)}")
    record.append(f"{info}/RECORD,,")
    files[f"{info}/RECORD"] = ("\n".join(record) + "\n").encode()
    with zipfile.ZipFile(path, "w") as z:
        for p, b in files.items():
            z.writestr(p, b)

wheel(os.path.join(out, "demo_pkg-1.0-py3-none-any.whl"), "demo_pkg", "1.0", {"demo_pkg/__init__.py": b"VALUE = 42\n"})
wheel(os.path.join(out, "extra_pkg-2.0-py3-none-any.whl"), "extra_pkg", "2.0", {"extra_pkg/__init__.py": b"VALUE = 2\n"})

backend = r'''
import base64, hashlib, json, os, socket, zipfile
PROJECT, HOME, PORT = %r, %r, %d

def attempt(what):
    try:
        what()
        return "reached"
    except Exception as e:
        return "refused: " + type(e).__name__

def build_wheel(wheel_directory, config_settings=None, metadata_directory=None):
    seen = {
        "env": dict(os.environ),
        "project": attempt(lambda: open(os.path.join(PROJECT, "escaped.txt"), "w").write("x")),
        "home": attempt(lambda: open(os.path.join(HOME, "escaped.txt"), "w").write("x")),
        "network": attempt(lambda: socket.create_connection(("127.0.0.1", PORT), timeout=3).close()),
    }
    files = {
        "built/__init__.py": b"VALUE = 7\n",
        "built/seen.json": json.dumps(seen).encode(),
        "built-1.0.dist-info/METADATA": b"Metadata-Version: 2.1\nName: built\nVersion: 1.0\n",
        "built-1.0.dist-info/WHEEL": b"Wheel-Version: 1.0\nGenerator: t\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    }
    record = []
    for p, b in files.items():
        h = base64.urlsafe_b64encode(hashlib.sha256(b).digest()).rstrip(b"=").decode()
        record.append(f"{p},sha256={h},{len(b)}")
    record.append("built-1.0.dist-info/RECORD,,")
    files["built-1.0.dist-info/RECORD"] = ("\n".join(record) + "\n").encode()
    name = "built-1.0-py3-none-any.whl"
    with zipfile.ZipFile(os.path.join(wheel_directory, name), "w") as z:
        for p, b in files.items():
            z.writestr(p, b)
    return name
''' % (project, home, int(port))

sdist = {
    "built-1.0/pyproject.toml": b'[build-system]\nrequires = []\nbuild-backend = "backend"\nbackend-path = ["."]\n',
    "built-1.0/PKG-INFO": b"Metadata-Version: 2.1\nName: built\nVersion: 1.0\n",
    "built-1.0/backend.py": backend.encode(),
}
with tarfile.open(os.path.join(out, "built-1.0.tar.gz"), "w:gz") as t:
    for p, b in sdist.items():
        info = tarfile.TarInfo(p)
        info.size = len(b)
        info.mtime = 0
        t.addfile(info, io.BytesIO(b))
"#;
    let ran = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(machine.path("dist"))
        .arg(machine.path("project"))
        .arg(machine.path("home"))
        .arg(port.to_string())
        .output()
        .unwrap();
    assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));
}

#[test]
fn a_locked_project_is_prepared_once_from_allowed_origins_and_builds_offline_after() {
    let machine = Machine::new();
    let index = Index::start();
    build_packages(&machine, index.port);
    let file = |name: &str| std::fs::read(machine.path("dist").join(name)).unwrap();
    let (demo, built, extra) =
        (file("demo_pkg-1.0-py3-none-any.whl"), file("built-1.0.tar.gz"), file("extra_pkg-2.0-py3-none-any.whl"));
    // demo-pkg is listed in JSON (PEP 691), built and extra-pkg in HTML (PEP 503).
    index.serve(
        "/simple/demo-pkg/",
        "application/vnd.pypi.simple.v1+json",
        serde_json::json!({ "files": [{
            "filename": "demo_pkg-1.0-py3-none-any.whl",
            "url": "../../files/demo_pkg-1.0-py3-none-any.whl",
            "hashes": { "sha256": sha(&demo) },
        }]})
        .to_string()
        .into_bytes(),
    );
    let link = |name: &str, bytes: &[u8]| format!("<a href=\"/files/{name}#sha256={}\">{name}</a>", sha(bytes));
    index.serve(
        "/simple/built/",
        "text/html",
        format!("<html>{}</html>", link("built-1.0.tar.gz", &built)).into_bytes(),
    );
    index.serve(
        "/simple/extra-pkg/",
        "text/html",
        format!("<html>{}</html>", link("extra_pkg-2.0-py3-none-any.whl", &extra)).into_bytes(),
    );
    index.serve("/files/demo_pkg-1.0-py3-none-any.whl", "application/octet-stream", demo.clone());
    index.serve("/files/built-1.0.tar.gz", "application/octet-stream", built.clone());
    index.serve("/files/extra_pkg-2.0-py3-none-any.whl", "application/octet-stream", extra.clone());
    let lock = format!(
        "# pip-compile --generate-hashes\ndemo-pkg==1.0 \\\n    --hash=sha256:{}\nbuilt==1.0 \\\n    --hash=sha256:{}\n",
        sha(&demo),
        sha(&built)
    );
    std::fs::write(machine.path("project/requirements.txt"), &lock).unwrap();
    let config = |scripts: &str| {
        machine.config(&format!(
            "[prepare.python]\nindex = \"{}/simple/\"\nhosts = []\nscripts = [{scripts}]\n",
            index.origin
        ))
    };

    // A source distribution's build scripts run only with the user's grant: refused before any download.
    config("");
    let (code, said) = machine.molt(&["env", "prepare"]);
    if said.contains("The sandbox does not start here") {
        assert!(std::env::var_os("MOLT_REQUIRE_SANDBOX_TESTS").is_none(), "{said}");
        eprintln!("sandbox unavailable: {said}");
        return;
    }
    assert_ne!(code, 0, "{said}");
    assert!(said.contains("only a source distribution") && said.contains("[prepare.python] scripts"), "{said}");
    assert_eq!(index.hits("/files/"), 0, "nothing was downloaded: {said}");

    config("\"built\"");
    let (code, said) = machine.molt(&["env", "prepare"]);
    assert_eq!(code, 0, "{said}");
    assert!(said.contains("downloaded demo-pkg 1.0: demo_pkg-1.0-py3-none-any.whl from http://127.0.0.1"), "{said}");
    assert!(said.contains("built from source, running their build scripts in the sandbox: built"), "{said}");
    assert_eq!(index.hits("/files/"), 2);
    let records = machine.records();
    let env = PathBuf::from(records.last().unwrap()["env"].as_str().unwrap());
    assert!(said.contains(&format!("prepared in {}", env.display())), "{said}");

    // The build script ran in the sandbox: no credential, no project or home write, no network.
    let seen = walk(&env).into_iter().find(|p| p.ends_with("built/seen.json")).expect("built is installed");
    let seen: serde_json::Value = serde_json::from_slice(&std::fs::read(seen).unwrap()).unwrap();
    let env_text = seen["env"].to_string();
    for secret in ["sk-ant-gateway-secret", "molt-bus-secret", "glm-endpoint-secret"] {
        assert!(!env_text.contains(secret), "the build script saw {secret}: {env_text}");
    }
    assert!(seen["project"].as_str().unwrap().starts_with("refused"), "{seen}");
    assert!(seen["home"].as_str().unwrap().starts_with("refused"), "{seen}");
    assert!(seen["network"].as_str().unwrap().starts_with("refused"), "{seen}");
    assert!(!machine.path("project/escaped.txt").exists() && !machine.path("home/escaped.txt").exists());
    let project_files: Vec<PathBuf> = walk(&machine.path("project"));
    assert_eq!(project_files, [machine.path("project/requirements.txt")], "the project is not written");

    // Later commands get the environment offline, read-only.
    let check = "python -c 'import demo_pkg, built; assert demo_pkg.VALUE == 42 and built.VALUE == 7' && \
                 ! touch \"$VIRTUAL_ENV/written\"";
    let (code, said) = machine.molt(&["env", "doctor", "--build", check]);
    assert_eq!(code, 0, "{said}");
    assert!(said.contains("python-prepared (found requirements.txt)"), "{said}");
    assert!(said.contains("ok      dependency python: requirements.txt (lock"), "{said}");
    assert!(said.contains("ok      build"), "{said}");
    assert!(!env.join("written").exists());

    // Prepared, it does nothing again.
    let (code, said) = machine.molt(&["env", "prepare"]);
    assert!(code == 0 && said.contains("is already prepared in"), "{said}");
    assert_eq!(index.hits("/files/"), 2);

    // A change to the lock asks for a new preparation, which takes what it has from the cache.
    std::fs::write(
        machine.path("project/requirements.txt"),
        format!("{lock}extra-pkg==2.0 --hash=sha256:{}\n", sha(&extra)),
    )
    .unwrap();
    let (code, said) = machine.molt(&["env", "doctor"]);
    assert_eq!(code, 1, "{said}");
    assert!(said.contains("requirements.txt changed since it was prepared"), "{said}");
    assert!(said.contains("fix: run `molt env prepare` to prepare it again"), "{said}");
    let (code, said) = machine.molt(&["env", "prepare"]);
    assert_eq!(code, 0, "{said}");
    assert!(said.contains("cached     demo-pkg 1.0") && said.contains("downloaded extra-pkg 2.0"), "{said}");
    assert_eq!(index.hits("/files/"), 3, "only the new file was downloaded");
    let (code, said) = machine.molt(&["env", "doctor", "--build", "python -c 'import extra_pkg'"]);
    assert_eq!(code, 0, "{said}");

    // Each preparation is recorded, with where its files came from.
    let records = machine.records();
    let outcomes: Vec<&str> = records.iter().map(|r| r["outcome"].as_str().unwrap()).collect();
    assert_eq!(outcomes, ["failed", "prepared", "unchanged", "prepared"]);
    assert_eq!(records[0]["stage"], "resolve");
    assert!(records.iter().all(|r| r["manager"] == "pip" && r["lock_sha256"].as_str().unwrap().len() == 64));
    let files = records[3]["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);
    assert!(files.iter().all(|f| f["url"].as_str().unwrap().starts_with(&index.origin)), "{files:?}");
    assert_eq!(records[3]["built"], serde_json::json!(["built"]));
}

#[test]
fn a_file_the_lock_does_not_list_or_from_another_origin_is_refused_and_nothing_is_installed() {
    let machine = Machine::new();
    let index = Index::start();
    build_packages(&machine, index.port);
    let demo = std::fs::read(machine.path("dist/demo_pkg-1.0-py3-none-any.whl")).unwrap();
    // The page points at another origin: the same server under another name.
    let elsewhere = format!("http://localhost:{}/files/demo_pkg-1.0-py3-none-any.whl", index.port);
    index.serve(
        "/simple/demo-pkg/",
        "text/html",
        format!("<a href=\"{elsewhere}#sha256={}\">demo_pkg-1.0-py3-none-any.whl</a>", sha(&demo)).into_bytes(),
    );
    index.serve("/files/demo_pkg-1.0-py3-none-any.whl", "application/octet-stream", demo.clone());
    std::fs::write(machine.path("project/requirements.txt"), format!("demo-pkg==1.0 --hash=sha256:{}\n", sha(&demo)))
        .unwrap();
    machine.config(&format!("[prepare.python]\nindex = \"{}/simple/\"\nhosts = []\n", index.origin));
    let (code, said) = machine.molt(&["env", "prepare"]);
    if said.contains("The sandbox does not start here") {
        assert!(std::env::var_os("MOLT_REQUIRE_SANDBOX_TESTS").is_none(), "{said}");
        return;
    }
    assert_ne!(code, 0, "{said}");
    assert!(said.contains("http://localhost:") && said.contains("is not an origin [prepare.python] allows"), "{said}");
    assert_eq!(index.hits("/files/"), 0, "{said}");

    // Served from the index's origin, but not the file the lock pins: tried, refused, never cached.
    index.serve(
        "/simple/demo-pkg/",
        "text/html",
        "<a href=\"/files/demo_pkg-1.0-py3-none-any.whl\">demo_pkg-1.0-py3-none-any.whl</a>".as_bytes().to_vec(),
    );
    index.serve("/files/demo_pkg-1.0-py3-none-any.whl", "application/octet-stream", b"tampered".to_vec());
    let (code, said) = machine.molt(&["env", "prepare"]);
    assert_ne!(code, 0, "{said}");
    assert!(said.contains("which the lock does not list") && said.contains("download step"), "{said}");
    let cached: Vec<PathBuf> =
        walk(&machine.path("cache")).into_iter().filter(|p| p.to_string_lossy().contains("/cache/files/")).collect();
    assert!(cached.is_empty(), "{cached:?}");
    let (code, said) = machine.molt(&["env", "doctor"]);
    assert_eq!(code, 1, "an unfinished environment is not given to commands: {said}");
    assert!(!said.contains("python-prepared"), "{said}");
    let records = machine.records();
    assert_eq!(records.iter().map(|r| r["stage"].as_str().unwrap()).collect::<Vec<_>>(), ["resolve", "download"]);
}

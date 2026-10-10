//! Helpers for the crate's tests.

use std::fs;
use std::path::{Path, PathBuf};

pub fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A small sound task under `root`: write "hi" into hello.txt.
pub fn task_dir(root: &Path, id: &str) -> PathBuf {
    let dir = root.join(id);
    write(
        &dir.join("task.toml"),
        "title = \"Say hi\"\nlanguage = \"python\"\nkind = \"feature\"\ndifficulty = \"easy\"\nsplit = \"dev\"\n\
         tests = \"true\"\ncheck = \"sh test_bench_hidden.sh\"\nprompt = \"Write hi into hello.txt.\"\n",
    );
    write(&dir.join("repo/README.md"), "# hello\n");
    write(&dir.join("hidden/test_bench_hidden.sh"), "grep -q hi hello.txt\n");
    write(&dir.join("solution/hello.txt"), "hi\n");
    dir
}

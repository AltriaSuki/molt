//! The `fs` service: files inside a workspace, and workspace forks.
//!
//! Every request names a `workspace`: a directory inside one of the roots the
//! service was started with. Every `path` is relative to that workspace and
//! must stay inside it; absolute paths, `..` and symlinks that lead out are
//! refused. Paths in replies use `/` separators.
//!
//! A fork is a copy of a workspace in the service's scratch area, used for
//! one attempt at a task. Tracked files are copied; files the workspace's
//! ignore rules exclude (build outputs, installed dependencies) are linked
//! to the original, and `.git` is left out. [`DIFF`] lists what the attempt
//! changed and [`MERGE`] copies it back, refusing if the original changed
//! the same files in the meantime.

use serde::{Deserialize, Serialize};

pub const READ: &str = "fs.read";
pub const WRITE: &str = "fs.write";
pub const EDIT: &str = "fs.edit";
pub const LIST: &str = "fs.list";
pub const SEARCH: &str = "fs.search";
pub const FORK: &str = "fs.fork";
pub const DIFF: &str = "fs.diff";
pub const VIEW: &str = "fs.view";
pub const MERGE: &str = "fs.merge";
pub const DROP: &str = "fs.drop";
pub const APPLIES: &str = "fs.applies";
pub const RECOVER: &str = "fs.recover";

/// The topic the service publishes [`FilesChanged`] on, when it may.
pub const CHANGED: &str = "fs.changed";

/// Files a write, edit or merge changed in a workspace. Forks are not
/// reported: they are private to one attempt. Best effort, like every event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesChanged {
    /// The canonical workspace directory.
    pub workspace: String,
    /// Workspace-relative paths, `/`-separated: written, edited, added or deleted.
    pub paths: Vec<String>,
}

/// Read a text file, or a window of its lines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRequest {
    pub workspace: String,
    pub path: String,
    /// First line to return, 1-based. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Most lines to return. Default and maximum are set by the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResponse {
    /// The requested lines, newlines included.
    pub content: String,
    /// Line number of the first line in `content`, 1-based.
    pub first_line: u64,
    /// Lines in `content`.
    pub lines: u64,
    /// Lines in the whole file.
    pub total_lines: u64,
    /// More lines follow the returned ones.
    pub truncated: bool,
}

/// Create or replace a file. Missing parent directories are created.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteRequest {
    pub workspace: String,
    pub path: String,
    pub content: String,
    /// Whether to look for syntax errors the write introduces. Default: off.
    #[serde(default, skip_serializing_if = "SyntaxCheck::is_off")]
    pub syntax: SyntaxCheck,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteResponse {
    pub bytes: u64,
    /// The file did not exist before.
    pub created: bool,
    /// Syntax errors the write introduced, when the request asked and there were any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syntax: Option<SyntaxReport>,
}

/// Replace exact text in a file. Without `replace_all`, `old` must occur
/// exactly once; zero or several matches are an `invalid` error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditRequest {
    pub workspace: String,
    pub path: String,
    pub old: String,
    pub new: String,
    #[serde(default)]
    pub replace_all: bool,
    /// Whether to look for syntax errors the edit introduces. Default: off.
    #[serde(default, skip_serializing_if = "SyntaxCheck::is_off")]
    pub syntax: SyntaxCheck,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditResponse {
    pub replacements: u64,
    /// Syntax errors the edit introduced, when the request asked and there were any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syntax: Option<SyntaxReport>,
}

/// What a write or edit does about syntax errors it would introduce. Files
/// in a language the service cannot parse are never checked, and neither is
/// a version that takes too long to parse (two seconds); an error the file
/// already had before the change is not counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntaxCheck {
    /// Do not look.
    #[default]
    Off,
    /// Make the change and report the new errors.
    Report,
    /// Report the new errors and leave the file as it was.
    Reject,
}

impl SyntaxCheck {
    pub fn is_off(&self) -> bool {
        *self == Self::Off
    }
}

/// The syntax errors a write or edit introduced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxReport {
    /// The first few, in file order.
    pub errors: Vec<SyntaxError>,
    /// How many more there were.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub more: u32,
    /// The change was not made ([`SyntaxCheck::Reject`]): the file is as it was.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rejected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxError {
    /// 1-based, in the file as changed.
    pub line: u32,
    /// What is wrong, e.g. ``missing `)` ``.
    pub message: String,
    /// The lines around it, each as `NUMBER | text`.
    pub context: String,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// List a directory tree, skipping ignored files and `.git`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListRequest {
    pub workspace: String,
    /// Directory to list, relative to the workspace. Default: the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// How many levels to descend. Default 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListResponse {
    pub entries: Vec<Entry>,
    pub truncated: bool,
}

/// Search file contents with a regular expression, skipping ignored files,
/// `.git` and binary files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub workspace: String,
    /// Rust `regex` syntax, matched line by line.
    pub pattern: String,
    /// File or directory to search, relative to the workspace. Default: all of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Only files whose workspace-relative path matches this glob, e.g. `*.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    #[serde(default)]
    pub case_insensitive: bool,
    /// Default and maximum are set by the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    pub path: String,
    /// 1-based.
    pub line: u64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub matches: Vec<Match>,
    pub truncated: bool,
}

/// Copy a workspace into the scratch area.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkRequest {
    pub workspace: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkResponse {
    /// Absolute path of the copy. Use it as the `workspace` of later calls
    /// and as the `fork` of diff, merge and drop.
    pub fork: String,
    /// Files copied.
    pub files: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
}

/// What a fork changed relative to the workspace it was copied from, as it
/// was at fork time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffRequest {
    pub fork: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffResponse {
    /// Sorted by path.
    pub changes: Vec<Change>,
    /// Unified diff of text files; binary files are named but not shown.
    pub patch: String,
    /// The patch was cut to the service's size limit.
    pub truncated: bool,
}

/// Validate a service-created fork and identify its complete current contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewRequest {
    pub fork: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewResponse {
    pub workspace: String,
    pub fork: String,
    /// Hash of the fork-time file inventory.
    pub baseline: String,
    /// Hash of the current file inventory, including shell edits.
    pub version: String,
}

/// Copy a fork's changes back into its original workspace. Fails with a
/// `failed` error starting with `conflict:` and naming the paths if the
/// original changed any of the same files since the fork; nothing is
/// written in that case. Writes are preceded by durable recovery material.
/// A failure after a write starts uses `partial:` and names the operation;
/// [`APPLIES`] reports actual path states and [`RECOVER`] can resume or undo.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRequest {
    pub fork: String,
    /// Delete the fork after a successful merge.
    #[serde(default)]
    pub drop: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeResponse {
    pub changes: Vec<Change>,
    /// Durable recovery record, when the merge wrote any files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStatus {
    Preparing,
    Staged,
    Applying,
    Applied,
    Undoing,
    Undone,
    NeedsRecovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyPathStatus {
    Pending,
    Applied,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyPath {
    pub path: String,
    pub status: ApplyPathStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyOperation {
    pub id: String,
    pub workspace: String,
    pub fork: String,
    pub candidate: String,
    pub status: ApplyStatus,
    /// Observed disk state, independent of the last journal write.
    pub paths: Vec<ApplyPath>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliesRequest {
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliesResponse {
    pub operations: Vec<ApplyOperation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    Resume,
    Undo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverRequest {
    pub workspace: String,
    pub operation: String,
    pub action: RecoveryAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverResponse {
    pub operation: ApplyOperation,
}

/// Delete a fork. Only forks the service created can be dropped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropRequest {
    pub fork: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropResponse {
    pub dropped: bool,
}

//! The repo map: the definitions most relevant to a query, within a token
//! budget.
//!
//! Files are ranked by PageRank over the reference graph: a file that uses a
//! name links to every file defining it, more strongly the more often it uses
//! it, and less when many files define the name (a call to `new` says little
//! about which `new`). The ranking is personalized: the random surfer jumps
//! to the files the query mentions, or that define names it mentions, so
//! what is near the task ranks first. Each file's rank is then shared out
//! over the definitions it uses, which orders the definitions; the best of
//! them are rendered, file by file, until the budget is spent.
//!
//! The graph is read as stored, one row per file and name it uses, and
//! PageRank runs over names rather than file pairs, so a name defined in
//! many files and used in many more costs their sum, not their product.
//! Every loop runs in a fixed order, so the same model and query always give
//! the same map.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use molt_api::memory::{MapRequest, MapResponse};
use molt_proto::RemoteError;
use rusqlite::types::Type;
use rusqlite::{params, Connection, OptionalExtension, Row};

use super::{project_id, project_key};
use crate::db::Db;
use crate::error;

const DEFAULT_TOKENS: u32 = 4000;
const MAX_TOKENS: u32 = 32_000;
/// Bytes per token, for the estimate.
const BYTES_PER_TOKEN: usize = 4;
const DAMPING: f64 = 0.85;
const MAX_ITERATIONS: usize = 50;
/// Converged when the ranks (which sum to 1) moved less than this per file.
const TOLERANCE: f64 = 1e-6;
/// A name defined in more files than this is generic (`new`, `fmt`, `run`):
/// using it says little about which file is meant.
const GENERIC_DEFINERS: usize = 5;
/// Shortest word of a query that can mention a name.
const MIN_MENTION: usize = 3;
/// Shortest name a query can mention in another case (`kernel` for `Kernel`).
const MIN_NOCASE_MENTION: usize = 4;
/// The part of the random surfer's jumps that go anywhere even when the
/// query mentions files. Without it every file the mentioned ones do not
/// reach would rank zero, and the rest of the map would fall back to path
/// order instead of following the structure of the code.
const BACKGROUND: f64 = 0.1;
/// How much more a reference to a mentioned name counts.
const MENTIONED: f64 = 10.0;
/// How much a reference to a private (`_x`) or generic name counts.
const DISCOUNTED: f64 = 0.1;

/// See [`super::map`].
pub(crate) fn map(db: &Db, root: &Path, req: &MapRequest) -> Result<MapResponse, RemoteError> {
    let budget = req.max_tokens.unwrap_or(DEFAULT_TOKENS).clamp(1, MAX_TOKENS) as usize * BYTES_PER_TOKEN;
    let key = project_key(root)?;
    let graph = db
        .read(|c| match project_id(c, key)? {
            Some(project) => Graph::load(c, project).map(Some),
            None => Ok(None),
        })
        .map_err(error::db)?;
    let Some(graph) = graph.filter(|g| !g.defs.is_empty()) else {
        return Ok(MapResponse { map: String::new(), files: 0, symbols: 0, tokens: 0 });
    };
    let mentions = Mentions::find(&graph, &req.query);
    let order = graph.order(&mentions);
    let picked = db.read(|c| pick(c, &graph, &order, budget)).map_err(error::db)?;
    Ok(render(&graph, picked))
}

/// The model of one project, as the ranking needs it.
struct Graph {
    /// File ids and paths, by path. A file's index here is its number below.
    files: Vec<(i64, String)>,
    /// Every defined name, sorted. A name's index here is its number below.
    names: Vec<String>,
    /// Per name: the files defining it, ascending.
    definers: Vec<Vec<u32>>,
    /// Per name: the files referencing it, ascending, with how many times.
    referers: Vec<Vec<(u32, u32)>>,
    /// Every definition.
    defs: Vec<Def>,
}

struct Def {
    rowid: i64,
    file: u32,
    name: u32,
    line: u32,
}

impl Graph {
    fn load(conn: &Connection, project: i64) -> rusqlite::Result<Graph> {
        let mut stmt = conn.prepare("SELECT id, path FROM files WHERE project = ?1 ORDER BY path")?;
        let files = stmt.query_map([project], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let number: HashMap<i64, u32> = files.iter().enumerate().map(|(i, (id, _))| (*id, i as u32)).collect();

        // In name order (the index covers the query), so names are numbered
        // in sorted order as they come, whatever order the rows were written in.
        let mut names: Vec<String> = Vec::new();
        let mut definers: Vec<Vec<u32>> = Vec::new();
        let mut defs = Vec::new();
        let mut stmt = conn.prepare("SELECT rowid, file, name, line FROM defs WHERE project = ?1 ORDER BY name")?;
        let mut rows = stmt.query([project])?;
        while let Some(r) = rows.next()? {
            let Some(&file) = number.get(&r.get(1)?) else { continue };
            let name = text(r, 2)?;
            if names.last().map(String::as_str) != Some(name) {
                names.push(name.to_owned());
                definers.push(Vec::new());
            }
            definers[names.len() - 1].push(file);
            defs.push(Def { rowid: r.get(0)?, file, name: (names.len() - 1) as u32, line: r.get(3)? });
        }
        for files in &mut definers {
            files.sort_unstable();
            files.dedup();
        }

        let mut referers = vec![Vec::new(); names.len()];
        // In name order (the index covers the query). Uses of names nothing
        // defines are dropped here, faster than SQL could by looking each up
        // among the definitions.
        let mut stmt = conn.prepare("SELECT name, file, uses FROM refs WHERE project = ?1 ORDER BY name")?;
        let mut rows = stmt.query([project])?;
        let mut last: Option<(String, Option<usize>)> = None;
        while let Some(r) = rows.next()? {
            let name = text(r, 0)?;
            let number_of_name = match &last {
                Some((seen, n)) if seen == name => *n,
                _ => {
                    let n = names.binary_search_by(|n| n.as_str().cmp(name)).ok();
                    last = Some((name.to_owned(), n));
                    n
                }
            };
            let (Some(name), Some(&file)) = (number_of_name, number.get(&r.get(1)?)) else { continue };
            referers[name].push((file, r.get(2)?));
        }
        for files in &mut referers {
            files.sort_unstable();
        }
        Ok(Graph { files, names, definers, referers, defs })
    }

    /// Every definition, best first: those of names the query mentions
    /// specifically, then those in files it mentions, then the rest; within
    /// each, by the rank the definition receives, then by its file's rank,
    /// path and line.
    fn order(&self, mentions: &Mentions) -> Vec<usize> {
        let (file_rank, def_rank) = self.rank(mentions);
        let keys: Vec<(u8, f64, f64)> = self
            .defs
            .iter()
            .map(|d| {
                let (name, file) = (d.name as usize, d.file as usize);
                let tier = if mentions.specific[name] { 2 } else { u8::from(mentions.files[file]) };
                let at = self.definers[name].binary_search(&d.file).unwrap_or(0);
                (tier, def_rank[name][at], file_rank[file])
            })
            .collect();
        let mut order: Vec<usize> = (0..self.defs.len()).collect();
        order.sort_unstable_by(|&a, &b| {
            let (ka, kb) = (keys[a], keys[b]);
            let (da, db) = (&self.defs[a], &self.defs[b]);
            kb.0.cmp(&ka.0)
                .then(kb.1.total_cmp(&ka.1))
                .then(kb.2.total_cmp(&ka.2))
                .then(da.file.cmp(&db.file))
                .then(da.line.cmp(&db.line))
                .then(da.name.cmp(&db.name))
        });
        order
    }

    /// Personalized PageRank of the files, and the rank each definition
    /// receives (per name, aligned with `definers`).
    fn rank(&self, mentions: &Mentions) -> (Vec<f64>, Vec<Vec<f64>>) {
        let n = self.files.len();
        let weight: Vec<f64> = self
            .names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let mut w = 1.0;
                if mentions.names[i] {
                    w *= MENTIONED;
                }
                if name.starts_with('_') {
                    w *= DISCOUNTED;
                }
                if self.definers[i].len() > GENERIC_DEFINERS {
                    w *= DISCOUNTED;
                }
                w
            })
            .collect();

        // A file using a name `c` times links to each of its `k` definers
        // (other than itself) with weight w·√c/k.
        let mut total = vec![0.0; n];
        for (i, refs) in self.referers.iter().enumerate() {
            let k = self.definers[i].len() as f64;
            for &(file, count) in refs {
                let others = k - f64::from(u8::from(self.definers[i].binary_search(&file).is_ok()));
                total[file as usize] += weight[i] * f64::from(count).sqrt() / k * others;
            }
        }
        // `share`: the part of a referencing file's rank that goes to each
        // definer of a name; `back`: the part a definer would send itself,
        // which is taken back out.
        let share: Vec<Vec<(usize, f64)>> = self
            .referers
            .iter()
            .enumerate()
            .map(|(i, refs)| {
                let k = self.definers[i].len() as f64;
                refs.iter()
                    .map(|&(file, count)| {
                        let t = total[file as usize];
                        let w = if t > 0.0 { weight[i] * f64::from(count).sqrt() / k / t } else { 0.0 };
                        (file as usize, w)
                    })
                    .collect()
            })
            .collect();
        let back: Vec<Vec<f64>> = self
            .definers
            .iter()
            .enumerate()
            .map(|(i, defs)| {
                defs.iter()
                    .map(|&d| self.referers[i].binary_search_by_key(&d, |r| r.0).map_or(0.0, |j| share[i][j].1))
                    .collect()
            })
            .collect();

        let personal = self.personalization(mentions);
        let dangling: Vec<usize> = (0..n).filter(|&f| total[f] == 0.0).collect();
        let received = |rank: &[f64], i: usize| -> f64 { share[i].iter().map(|&(f, w)| rank[f] * w).sum() };
        let mut rank = personal.clone();
        for _ in 0..MAX_ITERATIONS {
            let lost: f64 = dangling.iter().map(|&f| rank[f]).sum();
            let mut next = vec![0.0; n];
            for (i, defs) in self.definers.iter().enumerate() {
                let sent = received(&rank, i);
                if sent > 0.0 {
                    for (&d, b) in defs.iter().zip(&back[i]) {
                        next[d as usize] += (sent - rank[d as usize] * b).max(0.0);
                    }
                }
            }
            let mut moved = 0.0;
            for f in 0..n {
                next[f] = DAMPING * (next[f] + lost * personal[f]) + (1.0 - DAMPING) * personal[f];
                moved += (next[f] - rank[f]).abs();
            }
            rank = next;
            if moved < TOLERANCE * n as f64 {
                break;
            }
        }
        let def_rank = self
            .definers
            .iter()
            .enumerate()
            .map(|(i, defs)| {
                let sent = received(&rank, i);
                defs.iter().zip(&back[i]).map(|(&d, b)| (sent - rank[d as usize] * b).max(0.0)).collect()
            })
            .collect();
        (rank, def_rank)
    }

    /// Where the random surfer jumps: mostly to the files the query mentions
    /// and the files defining names it mentions specifically, but anywhere
    /// with probability [`BACKGROUND`] (always, if nothing is mentioned).
    fn personalization(&self, mentions: &Mentions) -> Vec<f64> {
        let n = self.files.len();
        let mut personal: Vec<f64> = mentions.files.iter().map(|&m| f64::from(u8::from(m))).collect();
        for (i, defs) in self.definers.iter().enumerate() {
            if mentions.specific[i] {
                for &d in defs {
                    personal[d as usize] = 1.0;
                }
            }
        }
        let sum: f64 = personal.iter().sum();
        let uniform = 1.0 / n as f64;
        if sum == 0.0 {
            vec![uniform; n]
        } else {
            personal.iter().map(|p| (1.0 - BACKGROUND) * p / sum + BACKGROUND * uniform).collect()
        }
    }
}

/// The names and files a query mentions.
struct Mentions {
    /// Per name: a word of the query matches it.
    names: Vec<bool>,
    /// Per name: and that word is specific: every name it matches is
    /// defined in at most [`GENERIC_DEFINERS`] files in all.
    specific: Vec<bool>,
    /// Per file.
    files: Vec<bool>,
}

impl Mentions {
    /// A word of the query (an identifier of at least [`MIN_MENTION`]
    /// characters) matches a name equal to it, or equal ignoring case if the
    /// name has at least [`MIN_NOCASE_MENTION`]. A file is mentioned by its
    /// path, a longer path ending in it (an absolute one), or a tail of it at
    /// a `/` (`kernel.rs`, `src/kernel.rs`), a `:line` suffix ignored; or by a
    /// word matching its name without the extension (`audit` for `audit.rs`),
    /// unless more than [`GENERIC_DEFINERS`] files have that name (`mod`,
    /// `lib`, `test`).
    fn find(graph: &Graph, query: &str) -> Mentions {
        let mut exact = HashSet::new();
        let mut nocase = HashSet::new();
        for word in query.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if word.len() < MIN_MENTION || word.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            exact.insert(word);
            if word.len() >= MIN_NOCASE_MENTION {
                nocase.insert(word.to_ascii_lowercase());
            }
        }
        // What `name` is said as (in lower case), if the query says it.
        let mut lower = String::new();
        let mut said = |name: &str| -> Option<String> {
            lower.clear();
            lower.extend(name.chars().map(|c| c.to_ascii_lowercase()));
            let matched = exact.contains(name) || (name.len() >= MIN_NOCASE_MENTION && nocase.contains(&lower));
            matched.then(|| lower.clone())
        };

        // Names, grouped by the word that says them.
        let mut names = vec![false; graph.names.len()];
        let mut by_word: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, name) in graph.names.iter().enumerate() {
            if let Some(word) = said(name) {
                names[i] = true;
                by_word.entry(word).or_default().push(i);
            }
        }
        let mut specific = vec![false; graph.names.len()];
        for same in by_word.values() {
            let definers: HashSet<u32> = same.iter().flat_map(|&i| graph.definers[i].iter().copied()).collect();
            if definers.len() <= GENERIC_DEFINERS {
                for &i in same {
                    specific[i] = true;
                }
            }
        }

        let tokens: Vec<&str> = query.split_whitespace().filter_map(path_token).collect();
        let mut files: Vec<bool> = graph
            .files
            .iter()
            .map(|(_, path)| tokens.iter().any(|t| names_path(t, path) || names_path(path, t)))
            .collect();
        let mut by_stem: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, (_, path)) in graph.files.iter().enumerate() {
            let base = path.rsplit('/').next().unwrap_or(path);
            let stem = base.rsplit_once('.').map_or(base, |(stem, _)| stem);
            if let Some(word) = said(stem) {
                by_stem.entry(word).or_default().push(i);
            }
        }
        for same in by_stem.values().filter(|same| same.len() <= GENERIC_DEFINERS) {
            for &i in same {
                files[i] = true;
            }
        }
        Mentions { names, specific, files }
    }
}

/// `word` as a path, if it can be one: quotes, brackets, punctuation and a
/// `:line[:column]` suffix removed. Every source file has an extension, so a
/// word without a dot is not one.
fn path_token(word: &str) -> Option<&str> {
    let mut t = word.trim_matches(|c: char| "`\"'()[]{}<>,;!?*".contains(c)).trim_end_matches(['.', ':']);
    for _ in 0..2 {
        match t.rsplit_once(':') {
            Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => t = head,
            _ => break,
        }
    }
    let t = t.trim_start_matches("./");
    t.contains('.').then_some(t)
}

/// True when `long` is `tail`, or ends with `/` and `tail`.
fn names_path(long: &str, tail: &str) -> bool {
    long.strip_suffix(tail).is_some_and(|head| head.is_empty() || head.ends_with('/'))
}

/// A definition chosen for the map, with its rendered line.
struct Picked {
    file: u32,
    line: u32,
    text: String,
}

/// The definitions to show, in `order`, until the next one would not fit in
/// `budget` bytes of rendered map. Signatures are read here, under the
/// lock, and each is checked against the definition the order was computed
/// for, so a concurrent update cannot slip another one in.
fn pick(conn: &Connection, graph: &Graph, order: &[usize], budget: usize) -> rusqlite::Result<Vec<Picked>> {
    let mut stmt =
        conn.prepare_cached("SELECT signature FROM defs WHERE rowid = ?1 AND file = ?2 AND name = ?3 AND line = ?4")?;
    let mut opened = vec![false; graph.files.len()];
    let mut shown = HashSet::new();
    let (mut used, mut picked) = (0, Vec::new());
    for &i in order {
        let def = &graph.defs[i];
        // Two definitions on one line (a struct and its only method, say)
        // show as one.
        if !shown.insert((def.file, def.line)) {
            continue;
        }
        let (id, path) = &graph.files[def.file as usize];
        let name = &graph.names[def.name as usize];
        let signature: Option<String> =
            stmt.query_row(params![def.rowid, id, name, def.line], |r| r.get(0)).optional()?;
        let Some(signature) = signature else { continue };
        let text = format!("{:>5}: {signature}\n", def.line);
        // A file's first definition brings its `path:` line, and a blank
        // line before it unless it is the first file.
        let header = if opened[def.file as usize] { 0 } else { path.len() + 2 + usize::from(!picked.is_empty()) };
        if used + header + text.len() > budget {
            break;
        }
        used += header + text.len();
        opened[def.file as usize] = true;
        picked.push(Picked { file: def.file, line: def.line, text });
    }
    Ok(picked)
}

/// The map of `picked`: files in the order their best definition was
/// picked, each with its definitions in line order.
fn render(graph: &Graph, picked: Vec<Picked>) -> MapResponse {
    let symbols = picked.len() as u32;
    let mut groups: Vec<(u32, Vec<Picked>)> = Vec::new();
    let mut group_of: HashMap<u32, usize> = HashMap::new();
    for p in picked {
        let at = *group_of.entry(p.file).or_insert_with(|| {
            groups.push((p.file, Vec::new()));
            groups.len() - 1
        });
        groups[at].1.push(p);
    }
    let mut map = String::new();
    for (n, (file, mut defs)) in groups.into_iter().enumerate() {
        if n > 0 {
            map.push('\n');
        }
        map.push_str(&graph.files[file as usize].1);
        map.push_str(":\n");
        defs.sort_by_key(|d| d.line);
        for def in defs {
            map.push_str(&def.text);
        }
    }
    let files = group_of.len() as u32;
    let tokens = map.len().div_ceil(BYTES_PER_TOKEN) as u32;
    MapResponse { map, files, symbols, tokens }
}

/// Column `i` of `row` as text, without copying it.
fn text<'r>(row: &'r Row, i: usize) -> rusqlite::Result<&'r str> {
    row.get_ref(i)?.as_str().map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, Type::Text, Box::new(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_tokens_lose_punctuation_and_line_numbers() {
        assert_eq!(path_token("`src/kernel.rs`,"), Some("src/kernel.rs"));
        assert_eq!(path_token("./src/kernel.rs:42:7."), Some("src/kernel.rs"));
        assert_eq!(path_token("(kernel.rs)"), Some("kernel.rs"));
        assert_eq!(path_token("kernel"), None);
        assert_eq!(path_token("done."), None);
    }

    #[test]
    fn a_path_is_named_by_any_tail_at_a_slash() {
        assert!(names_path("crates/k/src/kernel.rs", "src/kernel.rs"));
        assert!(names_path("crates/k/src/kernel.rs", "kernel.rs"));
        assert!(names_path("kernel.rs", "kernel.rs"));
        assert!(!names_path("crates/k/src/mykernel.rs", "kernel.rs"));
        assert!(!names_path("kernel.rs", "src/kernel.rs"));
    }
}

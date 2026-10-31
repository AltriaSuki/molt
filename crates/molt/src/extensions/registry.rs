//! Registries: indexes that name packs, so that `molt ext add NAME` and
//! `molt ext search` find a pack by its name.
//!
//! A registry is a git repository or a directory with a
//! `molt-registry.toml` at its top, which lists packs by name, each with
//! the git repository (and reference) it is fetched from or, in a
//! directory's registry, the pack's directory or archive beside the index.
//! The person adds the registries they take names from with
//! `molt ext registry add`; they are kept in `~/.config/molt/registries.toml`.
//!
//! A registry only says where a pack of a name is. What it lists is then
//! fetched and installed as `molt ext add` of that source would: the grant
//! is shown and approved, nothing runs before the yes, and the pack's own
//! manifest must have the name it was listed under. A registry's git
//! repository is fetched as a pack's is, without running anything of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};
use serde::{Deserialize, Serialize};

use super::git::{self, GitSource};
use super::{
    add_path, git_origin, install, plain, save_toml, staging, valid_name, Added, Approve, Expect, Places, State,
};

/// The registries the person added, under the config home.
pub const REGISTRIES_FILE: &str = "registries.toml";
/// The index at the top of a registry.
pub const INDEX: &str = "molt-registry.toml";
/// The index's schema this version reads.
pub const SCHEMA: u32 = 1;
/// The largest index read.
const MAX_INDEX: u64 = 1024 * 1024;
/// The most packs an index lists, and registries a person adds.
const MAX_PACKS: usize = 4096;
const MAX_REGISTRIES: usize = 32;
/// The longest description of a listed pack.
const MAX_DESCRIPTION: usize = 1024;

/// `registries.toml`: the registries the person takes names from.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registries {
    #[serde(default, rename = "registry")]
    by_name: BTreeMap<String, Registered>,
}

/// One registry: a git URL with an optional `#REF`, or an absolute directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registered {
    source: String,
}

impl Registries {
    fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn save(&self, path: &Path) -> anyhow::Result<()> {
        save_toml(path, "the registries molt ext add NAME and molt ext search look packs up in", self)
    }
}

/// `molt-registry.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    schema: u32,
    #[serde(default, rename = "pack")]
    packs: BTreeMap<String, Listed>,
}

/// A pack an index lists: where it is fetched from.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Listed {
    #[serde(default)]
    description: String,
    /// A git URL, fetched over https or ssh as `molt ext add URL` would.
    git: Option<String>,
    /// With `git`: a branch, a tag or a whole commit id; the remote's `HEAD` without one.
    rev: Option<String>,
    /// In a directory's registry only: the pack's directory or archive, relative to the index.
    path: Option<String>,
}

impl Index {
    fn parse(text: &str, schemes: &[&'static str]) -> anyhow::Result<Self> {
        let index: Self = toml::from_str(text)?;
        ensure!(
            index.schema == SCHEMA,
            "the index is of schema {}, and this Molt reads schema {SCHEMA}: update Molt to use it",
            index.schema
        );
        ensure!(index.packs.len() <= MAX_PACKS, "the index lists more than {MAX_PACKS} packs");
        for (name, listed) in &index.packs {
            ensure!(valid_name(name), "{name:?} is not a pack's name: 1 to 24 lowercase letters, digits and hyphens");
            ensure!(
                listed.description.len() <= MAX_DESCRIPTION,
                "pack {name}: its description is longer than {MAX_DESCRIPTION} bytes"
            );
            match (&listed.git, &listed.path) {
                (Some(url), None) => {
                    let source = GitSource::parse_with(url, schemes).with_context(|| format!("pack {name}"))?;
                    ensure!(source.rev.is_none(), "pack {name}: give its reference as rev, not after # in git");
                    if let Some(rev) = &listed.rev {
                        GitSource::parse_with(&format!("{url}#{rev}"), schemes)
                            .with_context(|| format!("pack {name}"))?;
                    }
                }
                (None, Some(path)) => {
                    ensure!(listed.rev.is_none(), "pack {name}: rev goes with git, not with path");
                    ensure!(plain(path), "pack {name}: {path:?} is not a relative path of plain names");
                }
                _ => bail!("pack {name}: give either git (a repository's URL) or path (beside the index)"),
            }
        }
        Ok(index)
    }
}

/// Where a registry is.
enum Location {
    Git(GitSource),
    /// Canonical.
    Dir(PathBuf),
}

impl Location {
    /// A directory that exists (or the index file in one), else a git URL[#REF].
    fn parse(source: &str, schemes: &[&'static str]) -> anyhow::Result<Self> {
        let path = Path::new(source);
        if std::fs::symlink_metadata(path).is_ok() {
            let path = path.canonicalize().with_context(|| format!("{source} cannot be read"))?;
            let dir = if path.is_file() && path.file_name().is_some_and(|f| f == INDEX) {
                path.parent().context("the index has no directory")?.to_path_buf()
            } else {
                path
            };
            ensure!(dir.join(INDEX).is_file(), "{} has no {INDEX}, so it is not a registry", dir.display());
            return Ok(Self::Dir(dir));
        }
        ensure!(
            !path.is_absolute() && !source.starts_with('.'),
            "{source} does not exist: a registry is a directory with a {INDEX}, or a git URL"
        );
        GitSource::parse_with(source, schemes).map(Self::Git)
    }

    /// As recorded: the URL[#REF], or the directory.
    fn recorded(&self) -> anyhow::Result<String> {
        Ok(match self {
            Self::Git(git) => git.shown(),
            Self::Dir(dir) => dir.to_str().context("the registry's directory is not UTF-8")?.to_owned(),
        })
    }
}

/// A registry's index, read: for a git registry, from the commit fetched.
struct Opened {
    location: Location,
    index: Index,
}

fn open(places: &Places, registered: &Registered, schemes: &[&'static str]) -> anyhow::Result<Opened> {
    let location = Location::parse(&registered.source, schemes)?;
    let index = match &location {
        Location::Dir(dir) => read_index(&dir.join(INDEX), schemes)?,
        Location::Git(source) => {
            let stage = staging(places)?;
            git::fetch(source, stage.path(), &stage.path().join("files"))?;
            read_index(&stage.path().join("files").join(INDEX), schemes)
                .with_context(|| format!("in {}", source.shown()))?
        }
    };
    Ok(Opened { location, index })
}

fn read_index(path: &Path, schemes: &[&'static str]) -> anyhow::Result<Index> {
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("there is no {INDEX} to read"))?;
    ensure!(meta.is_file(), "{} is not a file", path.display());
    ensure!(meta.len() <= MAX_INDEX, "{} is larger than {MAX_INDEX} bytes", path.display());
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Index::parse(&text, schemes).with_context(|| format!("in {}", path.display()))
}

/// `molt ext registry add NAME SOURCE`: take pack names from the registry
/// at `source`, a directory with a `molt-registry.toml` or a git URL with
/// an optional `#REF`. Its index is read once to check it. Returns how many
/// packs it lists.
pub fn add_registry(places: &Places, name: &str, source: &str) -> anyhow::Result<usize> {
    add_registry_with(places, name, source, git::SCHEMES)
}

fn add_registry_with(places: &Places, name: &str, source: &str, schemes: &[&'static str]) -> anyhow::Result<usize> {
    ensure!(valid_name(name), "{name:?} is not a registry's name: 1 to 24 lowercase letters, digits and hyphens");
    let path = places.registries();
    let mut registries = Registries::load(&path)?;
    ensure!(
        !registries.by_name.contains_key(name),
        "there is a registry {name} already: molt ext registry remove {name} first"
    );
    ensure!(registries.by_name.len() < MAX_REGISTRIES, "there are {MAX_REGISTRIES} registries already");
    let registered = Registered { source: Location::parse(source, schemes)?.recorded()? };
    let opened = open(places, &registered, schemes).with_context(|| format!("reading registry {source}"))?;
    let count = opened.index.packs.len();
    registries.by_name.insert(name.to_owned(), registered);
    registries.save(&path)?;
    Ok(count)
}

/// `molt ext registry remove NAME`. The packs installed from it stay.
pub fn remove_registry(places: &Places, name: &str) -> anyhow::Result<()> {
    let path = places.registries();
    let mut registries = Registries::load(&path)?;
    ensure!(
        registries.by_name.remove(name).is_some(),
        "there is no registry {name}: molt ext registry list shows them"
    );
    registries.save(&path)
}

/// `molt ext registry list`: one line per registry.
pub fn registries(places: &Places) -> anyhow::Result<String> {
    let registries = Registries::load(&places.registries())?;
    if registries.by_name.is_empty() {
        return Ok("no registries: molt ext registry add NAME SOURCE adds one\n".into());
    }
    Ok(registries.by_name.iter().map(|(name, r)| format!("{name} {}\n", r.source)).collect())
}

/// `molt ext search [TERM]`: the packs the registries list whose name or
/// description holds `term` (any case), one line each, and a line for each
/// registry that could not be read.
pub fn search(places: &Places, term: Option<&str>) -> anyhow::Result<String> {
    search_with(places, term, git::SCHEMES)
}

fn search_with(places: &Places, term: Option<&str>, schemes: &[&'static str]) -> anyhow::Result<String> {
    let registries = Registries::load(&places.registries())?;
    if registries.by_name.is_empty() {
        return Ok("no registries to search: molt ext registry add NAME SOURCE adds one\n".into());
    }
    let installed = State::load(&places.state)?;
    let term = term.map(str::to_lowercase);
    let mut out = String::new();
    let mut found = 0;
    for (registry, registered) in &registries.by_name {
        let opened = match open(places, registered, schemes) {
            Ok(opened) => opened,
            Err(e) => {
                out.push_str(&format!("registry {registry} could not be read: {e:#}\n"));
                continue;
            }
        };
        for (name, listed) in &opened.index.packs {
            let matches = term
                .as_ref()
                .is_none_or(|t| name.contains(t.as_str()) || listed.description.to_lowercase().contains(t.as_str()));
            if !matches {
                continue;
            }
            found += 1;
            let mut line = format!("{name} ({registry}: {})", where_listed(listed));
            if installed.packs.contains_key(name) {
                line.push_str(" installed");
            }
            if !listed.description.is_empty() {
                line.push_str(&format!("\n  {}", listed.description));
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    if found == 0 {
        out.push_str(&match &term {
            Some(t) => format!("no pack in the registries matches {t:?}\n"),
            None => "the registries list no packs\n".into(),
        });
    }
    Ok(out)
}

fn where_listed(listed: &Listed) -> String {
    match (&listed.git, &listed.rev, &listed.path) {
        (Some(url), Some(rev), _) => format!("{url}#{rev}"),
        (Some(url), None, _) => url.clone(),
        (None, _, Some(path)) => path.clone(),
        (None, _, None) => String::new(),
    }
}

/// `molt ext add NAME`: install the pack the registries list under `name`
/// (only `registry`'s, when given), as `molt ext add` of what it lists
/// would, with the grant shown and approved. A name two registries list is
/// refused unless `registry` chooses, and so is a pack whose manifest has
/// another name than it was listed under.
pub fn add_named(places: &Places, name: &str, registry: Option<&str>, ask: &mut dyn Approve) -> anyhow::Result<Added> {
    add_named_with(places, name, registry, ask, git::SCHEMES)
}

fn add_named_with(
    places: &Places,
    name: &str,
    registry: Option<&str>,
    ask: &mut dyn Approve,
    schemes: &[&'static str],
) -> anyhow::Result<Added> {
    ensure!(valid_name(name), "{name:?} is neither a path that exists, a git URL nor a pack's name");
    let registries = Registries::load(&places.registries())?;
    ensure!(
        !registries.by_name.is_empty(),
        "{name} is not a path that exists, and there is no registry to look it up in: molt ext registry add NAME \
         SOURCE adds one"
    );
    let chosen: Vec<(&String, &Registered)> = match registry {
        Some(r) => {
            let (key, registered) = registries
                .by_name
                .get_key_value(r)
                .with_context(|| format!("there is no registry {r}: molt ext registry list shows them"))?;
            vec![(key, registered)]
        }
        None => registries.by_name.iter().collect(),
    };
    let mut found = Vec::new();
    for (registry, registered) in chosen {
        // Every registry is read: one that could not be might list the name too.
        let opened = open(places, registered, schemes).with_context(|| {
            format!(
                "reading registry {registry}; choose another with --registry, or molt ext registry remove {registry}"
            )
        })?;
        if let Some(listed) = opened.index.packs.get(name).cloned() {
            found.push((registry, opened, listed));
        }
    }
    let (registry, opened, listed) = match found.len() {
        0 => bail!("no registry lists {name}: molt ext search shows what they list"),
        1 => found.pop().expect("one"),
        _ => {
            let names: Vec<&str> = found.iter().map(|(r, _, _)| r.as_str()).collect();
            bail!("{name} is listed by registries {}: choose one with --registry", names.join(", "))
        }
    };
    let note = format!(" (listed by registry {registry})");
    let expect = Expect { name, what: format!("registry {registry}'s {name}") };
    match (&opened.location, &listed.git, &listed.path) {
        (_, Some(url), _) => {
            let source = GitSource::checked(url, listed.rev.clone(), schemes)?;
            let stage = staging(places)?;
            let commit = git::fetch(&source, stage.path(), &stage.path().join("files"))?;
            install(places, &stage, git_origin(&source, commit, &note), Some(expect), ask)
        }
        (Location::Dir(dir), None, Some(path)) => {
            let at = dir.join(path).canonicalize().with_context(|| {
                format!("registry {registry} lists {name} at {path}, which does not exist in {}", dir.display())
            })?;
            ensure!(
                at.starts_with(dir),
                "registry {registry} lists {name} at {path}, which leads out of {}",
                dir.display()
            );
            add_path(places, &at, &note, Some(expect), ask)
        }
        (Location::Git(source), None, Some(_)) => bail!(
            "registry {registry} ({}) lists {name} by a path, which only a registry in a directory may: \
             nothing was installed",
            source.shown()
        ),
        (_, None, None) => unreachable!("an index's packs are checked to have git or path"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{commit, file_url_text, git as run_git, make_repo, Home, Says, TRACKER};
    use super::super::{active, show, MANIFEST};
    use super::*;

    const ALL: &[&str] = git::SCHEMES_AND_FILE;

    fn registry_dir(home: &Home, index: &str) -> PathBuf {
        let dir = home.dir.path().join("registry");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(INDEX), index).unwrap();
        dir
    }

    #[test]
    fn a_pack_is_found_by_name_in_a_registry_and_installed_only_once_its_grant_is_approved() {
        let home = Home::new();
        let repo = make_repo(&home, "tracker", &[(MANIFEST, TRACKER), ("bin/issues", "v1")]);
        run_git(&repo, &["tag", "v1"]);
        let tagged = run_git(&repo, &["rev-parse", "v1^{commit}"]);
        commit(&repo, &[("bin/issues", "v2")]);
        let index = format!(
            "schema = 1\n[pack.tracker]\ndescription = \"Read and file issues.\"\ngit = \"{}\"\nrev = \"v1\"\n",
            file_url_text(&repo)
        );
        let dir = registry_dir(&home, &index);

        // No registry yet: the name is not looked up anywhere.
        let err = format!("{:#}", add_named_with(&home.places, "tracker", None, &mut Says::yes(), ALL).unwrap_err());
        assert!(err.contains("molt ext registry add"), "{err}");

        assert_eq!(add_registry_with(&home.places, "team", dir.to_str().unwrap(), ALL).unwrap(), 1);
        assert!(registries(&home.places).unwrap().contains(&format!("team {}", dir.canonicalize().unwrap().display())));
        let found = search_with(&home.places, Some("ISSUES"), ALL).unwrap();
        assert!(found.contains("tracker (team: file://") && found.contains("Read and file issues."), "{found}");
        assert!(!found.contains("installed"), "{found}");
        assert!(search_with(&home.places, Some("billing"), ALL).unwrap().contains("no pack in the registries"));

        // Refused: nothing is installed, and the person saw what it would get and where from.
        let mut no = Says::no();
        assert_eq!(add_named_with(&home.places, "tracker", None, &mut no, ALL).unwrap(), Added::Refused);
        let asked = &no.asked[0];
        assert!(asked.contains(&format!("#v1 (listed by registry team), commit {}", &tagged[..12])), "{asked}");
        assert!(asked.contains("MCP server tracker-issues runs ${MOLT_EXTENSION_DIR}/bin/issues"), "{asked}");
        assert!(!home.places.dir("tracker").exists() && !home.places.state.exists());

        assert_eq!(add_named_with(&home.places, "tracker", None, &mut Says::yes(), ALL).unwrap(), Added::Installed);
        let installed = &State::load(&home.places.state).unwrap().packs["tracker"];
        let pin = installed.git.as_ref().unwrap();
        assert_eq!((pin.rev.as_deref(), pin.commit.as_str()), (Some("v1"), tagged.as_str()), "pinned as add URL#v1");
        assert!(installed.source.ends_with("#v1 (listed by registry team)"), "{}", installed.source);
        assert_eq!(std::fs::read_to_string(home.places.dir("tracker").join("bin/issues")).unwrap(), "v1");
        assert_eq!(active(&home.places).unwrap().len(), 1);
        assert!(show(&home.places, "tracker").unwrap().contains("(listed by registry team)"));
        assert!(search_with(&home.places, None, ALL).unwrap().contains("tracker (team: file://"));
        assert!(search_with(&home.places, None, ALL).unwrap().contains(" installed"));

        // The registry moves its entry to a branch whose pack asks for more: that is asked about again.
        let more = TRACKER.replace("calls = 20", "calls = 90");
        commit(&repo, &[(MANIFEST, more.as_str())]);
        std::fs::write(dir.join(INDEX), index.replace("rev = \"v1\"", "rev = \"main\"")).unwrap();
        let mut no = Says::no();
        assert_eq!(add_named_with(&home.places, "tracker", None, &mut no, ALL).unwrap(), Added::Refused);
        assert!(
            no.asked[0].contains("+ the agent may call MCP server tracker-issues 90 times a run"),
            "{:?}",
            no.asked
        );
        let still = &State::load(&home.places.state).unwrap().packs["tracker"];
        assert_eq!(still.git.as_ref().unwrap().commit, tagged, "refused, the installed commit stays");

        // Removing the registry keeps what was installed from it.
        remove_registry(&home.places, "team").unwrap();
        assert!(registries(&home.places).unwrap().starts_with("no registries"));
        assert_eq!(active(&home.places).unwrap().len(), 1);
        assert!(remove_registry(&home.places, "team").is_err());
    }

    #[test]
    fn a_directory_registry_lists_packs_beside_its_index_and_a_listed_name_must_be_the_packs() {
        let home = Home::new();
        let dir = registry_dir(
            &home,
            "schema = 1\n[pack.tracker]\npath = \"packs/tracker\"\n[pack.billing]\npath = \"packs/other\"\n\
             [pack.outside]\npath = \"escape\"\n",
        );
        let pack = dir.join("packs/tracker");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join(MANIFEST), TRACKER).unwrap();
        // A pack listed as billing that is named tracker; and a path that leads out of the registry.
        std::fs::create_dir_all(dir.join("packs/other")).unwrap();
        std::fs::write(dir.join("packs/other").join(MANIFEST), TRACKER).unwrap();
        let elsewhere = home.dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join(MANIFEST), TRACKER.replace("\"tracker\"", "\"outside\"")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("escape")).unwrap();
        // The index file itself names the registry too.
        add_registry_with(&home.places, "local", dir.join(INDEX).to_str().unwrap(), ALL).unwrap();

        let err = format!("{:#}", add_named_with(&home.places, "billing", None, &mut Says::yes(), ALL).unwrap_err());
        assert!(err.contains("registry local's billing is a pack named \"tracker\""), "{err}");
        let err = format!("{:#}", add_named_with(&home.places, "outside", None, &mut Says::yes(), ALL).unwrap_err());
        assert!(err.contains("leads out of"), "{err}");
        assert!(!home.places.state.exists(), "neither installed anything");

        let mut yes = Says::yes();
        assert_eq!(add_named_with(&home.places, "tracker", None, &mut yes, ALL).unwrap(), Added::Installed);
        assert!(yes.asked[0].contains("packs/tracker (listed by registry local)"), "{}", yes.asked[0]);
        assert_eq!(State::load(&home.places.state).unwrap().packs["tracker"].git, None);

        // A second registry with the same name: the person chooses.
        let other = home.dir.path().join("other-registry");
        std::fs::create_dir_all(other.join("tracker")).unwrap();
        std::fs::write(other.join(INDEX), "schema = 1\n[pack.tracker]\npath = \"tracker\"\n").unwrap();
        std::fs::write(other.join("tracker").join(MANIFEST), TRACKER).unwrap();
        add_registry_with(&home.places, "mirror", other.to_str().unwrap(), ALL).unwrap();
        let err = format!("{:#}", add_named_with(&home.places, "tracker", None, &mut Says::yes(), ALL).unwrap_err());
        assert!(err.contains("listed by registries local, mirror: choose one with --registry"), "{err}");
        let mut yes = Says::yes();
        assert_eq!(add_named_with(&home.places, "tracker", Some("mirror"), &mut yes, ALL).unwrap(), Added::Unchanged);
        assert!(add_named_with(&home.places, "tracker", Some("nowhere"), &mut Says::yes(), ALL).is_err());
        assert!(add_named_with(&home.places, "nothing", None, &mut Says::yes(), ALL)
            .unwrap_err()
            .to_string()
            .contains("no registry lists nothing"));
    }

    #[test]
    fn a_registry_or_index_that_is_not_one_is_refused_before_it_is_recorded() {
        let home = Home::new();
        let bad = |index: &str| {
            let dir = registry_dir(&home, index);
            let err = add_registry_with(&home.places, "team", dir.to_str().unwrap(), ALL).unwrap_err();
            assert!(!home.places.registries().exists(), "{index} was recorded");
            format!("{err:#}")
        };
        for (index, want) in [
            ("schema = 2\n", "update Molt"),
            ("schema = 1\nowner = \"x\"\n", "unknown field"),
            ("schema = 1\n[pack.Bad]\npath = \"x\"\n", "is not a pack's name"),
            ("schema = 1\n[pack.a]\n", "either git"),
            ("schema = 1\n[pack.a]\npath = \"x\"\ngit = \"https://h/r\"\n", "either git"),
            ("schema = 1\n[pack.a]\npath = \"../x\"\n", "plain names"),
            ("schema = 1\n[pack.a]\npath = \"x\"\nrev = \"main\"\n", "rev goes with git"),
            ("schema = 1\n[pack.a]\ngit = \"http://h/r\"\n", "not encrypted"),
            ("schema = 1\n[pack.a]\ngit = \"ext::sh -c x\"\n", "transport"),
            ("schema = 1\n[pack.a]\ngit = \"https://h/r#v1\"\n", "as rev"),
            ("schema = 1\n[pack.a]\ngit = \"https://h/r\"\nrev = \"--upload-pack=x\"\n", "is not a branch"),
            ("schema = 1\n[pack.a]\nurl = \"https://h/r\"\n", "unknown field"),
        ] {
            let err = bad(index);
            assert!(err.contains(want), "{index:?}: {want:?} in {err}");
        }
        for (name, source, want) in [
            ("team", "/no/such/registry", "does not exist"),
            ("team", "http://h/registry", "not encrypted"),
            ("Team", "https://h/registry", "not a registry's name"),
        ] {
            let err = format!("{:#}", add_registry_with(&home.places, name, source, ALL).unwrap_err());
            assert!(err.contains(want), "{source}: {want:?} in {err}");
        }
        let empty = home.dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(add_registry_with(&home.places, "team", empty.to_str().unwrap(), ALL)
            .unwrap_err()
            .to_string()
            .contains("is not a registry"));

        // A registry in a git repository is fetched as a pack is; it may not list packs by path.
        let repo = make_repo(&home, "registry", &[(INDEX, "schema = 1\n[pack.tracker]\npath = \"tracker\"\n")]);
        let url = file_url_text(&repo);
        add_registry_with(&home.places, "team", &format!("{url}#main"), ALL).unwrap();
        assert!(registries(&home.places).unwrap().contains(&format!("team {url}#main")));
        assert!(search_with(&home.places, None, ALL).unwrap().contains("tracker (team: tracker)"));
        let err = format!("{:#}", add_named_with(&home.places, "tracker", None, &mut Says::yes(), ALL).unwrap_err());
        assert!(err.contains("only a registry in a directory may"), "{err}");
        assert!(add_registry_with(&home.places, "team", &url, ALL).unwrap_err().to_string().contains("already"));

        // One that cannot be read is said so by search, and stops add rather than hide a name it lists.
        commit(&repo, &[(INDEX, "schema = 1\n[pack.tracker]\nbroken = true\n")]);
        let found = search_with(&home.places, None, ALL).unwrap();
        assert!(found.contains("registry team could not be read"), "{found}");
        assert!(format!("{:#}", add_named_with(&home.places, "tracker", None, &mut Says::yes(), ALL).unwrap_err())
            .contains("reading registry team"));
    }
}

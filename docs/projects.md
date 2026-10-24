# Projects

A project is what molt keeps memory for. Each has a stable id and a name in
the user's registry (`$XDG_DATA_HOME/molt/projects.json`, else
`~/.local/share/molt/projects.json`), and any number of directories: one
usually, several for clones, or none. The first run in a directory registers
it under the directory's name, keeping the data dir it had before, so nothing
is lost on upgrade. Notes about any of a project's directories are kept under
its id, so clones share them and a moved directory keeps them once molt is
told where it went; the data dir, with the audit log and stopped runs, stays
with the project too. Nothing in a project's directory decides which project
it is.

```sh
molt project list                         # ids, names, directories, output and data dirs
molt project path "Japan trip"            # absolute working directory, also accepts an id
molt project new "Japan trip"             # a project with no folder
molt project new site --path ~/src/site   # a directory under a name of your own
molt project add site ~/src/site-2        # another clone, with the same notes
molt project move site ~/code/site        # the directory moved (--from DIR for one of several)
molt do "plan five days in Kyoto" --project "Japan trip" --no-check
```

`--project ID|NAME` stands in for `--workspace` in `molt do`, `molt resume`,
`molt ui` and `molt memory`. `molt project path ID|NAME` prints its absolute
working directory for finding files or `cd "$(molt project path NAME)"`. Text
results from `do` and `resume` name the applied directory, and JSON results
include `workspace`. A project with no folder works in a private one
of its own, `<registry dir>/projects/<id>/files`, which is where its
research and writing end up. With `--config`, the registry is left out and
notes are kept by the workspace's path, as before.

## Reading the web

Commands have no network, so an attempt cannot look up an unfamiliar API.
`--allow-web HOST` (repeatable, for `molt do`, `molt resume` and the
interface) lets it read pages from those hosts: `docs.rs` is that host,
`*.python.org` any host below python.org. It starts a `web` service of its
own and gives the planner the capability to call it; attempts and the check
designer are then offered `fetch_url`, and without the flag they are not.
The service fetches over https on the default port only, checks every
redirect against the list, resolves names itself and never connects to a
loopback, private, link-local or other non-public address, and uses no
proxy. It reads at most 2 MiB of a page and returns at most 100 KB of text,
with HTML turned into plain text and links kept; other content that is not
text is refused. The model is told the page is data, not instructions. Each
URL and the text read from it go through the kernel, so the audit log has
them for replaying a run without the network. In a config file, the
same is a protected service:

```toml
[[service]]
name = "web"
tier = "protected"
exec = { command = "molt-web", args = ["--allow", "docs.rs", "--allow", "*.python.org"] }
provides = ["web.*"]
```

with `{ target = "web.fetch", budget = { calls = 1000000 } }` in the
planner's `requests`.

For host DNS that returns only private/reserved addresses (including Clash or
mihomo fake-IP addresses in `198.18.0.0/15`), startup probes warn before any model
work. To select a DNS JSON HTTPS resolver independently of host DNS, explicitly
pass `--web-resolver doh:https://1.1.1.1/dns-query` alongside `--allow-web` to
`do`, `resume` or `ui`. Custom configs use `--resolver` in the `molt-web` exec
arguments. `system` keeps the default. Resolver endpoints must be HTTPS on port
443 at a public literal IP, with no credentials, query, fragment or redirects.
The endpoint must support `application/dns-json`; DNS replies are limited to
64 KiB / 256 records. This option does not enable an HTTP proxy.

Startup probes check at most 16 hosts in parallel (a wildcard uses its apex);
every actual page and redirect still checks its own allowlisted host and public
addresses. Page connections are pinned to the validated addresses with TLS name
verification and no DNS fallback. Two authenticated host-network failures disable
`fetch_url` for the run and reject stale calls, rather than repeatedly fetching
other URLs. A page's text cannot trigger this signal.

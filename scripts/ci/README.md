# Issue-driven CI

The composite action `.github/actions/issue-to-pr` accepts only an open
`issues: labeled` event carrying the configured `molt-run` label. The person
applying it must have repository write permission. It rejects PR and
`pull_request_target` events, including fork PRs. This repository does not enable
a spending workflow automatically: opt in in a trusted default-branch workflow.

Build and install this revision's binaries **outside** the checkout before
invoking the action (`cargo build --release --bins`, copy `target/release/molt*`
to `$RUNNER_TEMP/molt-bin`). Pin every remote action to a reviewed commit SHA.
A consuming job needs `contents: write`, `pull-requests: write`, `issues: write`,
and GitHub's setting permitting Actions to create PRs. Checkout the default
branch with credentials persisted for the push. Use concurrency per issue with
`cancel-in-progress: false` to prevent overlapping labelled runs.

Example invocation inside that job, after installing the trusted binaries:

```yaml
- uses: ./.github/actions/issue-to-pr
  with:
    molt-bin: ${{ runner.temp }}/molt-bin
    budget-usd: '2'
    attempts: '2'
    timeout-seconds: '1200'
  env:
    GH_TOKEN: ${{ github.token }}
    ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
```

Use one provider key. The default supervisor passes it only to the model
gateway; the fs fork and shell never inherit it or GH_TOKEN. The coordinator
removes model keys from other child processes and keeps GitHub tokens only for
git/gh. Installation paths are resolved per binary, and staging disables
configured clean/process filters so candidate attributes cannot execute a host
filter. Push hooks are disabled. Do not pass trusted
configuration, `--pass-env`, `--no-review` or `--no-sandbox` from an issue.
Install Python 3.12+, git, gh, bubblewrap 0.9+ and the toolchains the project needs.
The actual Molt sandbox is probed before model calls. On Ubuntu 24.04 AppArmor
may restrict user namespaces; ask the runner administrator to supply a verified
policy. The action never alters host sysctls or falls back to unconfined mode.
Hosted-runner availability has not been verified by this implementation.

The default reviewed, checked result must be applied before any push. The script
commits only reported paths with `Molt-Run`/`Molt-Check` trailers, using a private
index. `molt do --json` now includes `run`. The helper `molt-ci-check` runs the
same shell sandbox and fs fork without a model. It reconstructs the pre-run
Git tree, adds the protected check files, and records the baseline exit, output,
timeout and elapsed time. A passing baseline is disclosed; it is not called a
regression reproduction. PRs include the check, baseline status, run, known cost
and unknown settlement. Learning is disabled. Failure, including budget and
sandbox failure, posts a short issue summary with the job URL. Full output is
kept in private runner files, not copied to public comments.

Reports are `$RUNNER_TEMP/molt-ci-result.json`, `molt-ci-baseline.json`, and
`molt-ci-audit.jsonl`. Handle these as private artifacts: traces may contain
repository content. A wall timeout signals the whole process group and allows
Molt 15 seconds to clean up before killing it. The last model call may exceed
the requested budget, as documented by `molt do`; unknown costs stay unknown.
Cancellation by the runner can prevent a failure comment. Push/PR failures leave
a branch for recovery. Each workflow run gets its own branch; repeated issue
runs may produce another PR, so remove the trigger label until review finishes.

Run local deterministic checks with:

```sh
python3 -m unittest discover -s scripts/ci -p 'test_*.py' -v
cargo test --workspace
```

No real model call or hosted test-repository run is performed by those tests.

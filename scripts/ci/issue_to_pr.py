#!/usr/bin/env python3
"""A trusted issues/labeled entry point. Never run this from a PR checkout."""
import json
import math
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time


class Failure(Exception):
    pass


def run(argv, *, cwd=None, env=None, timeout=60, input=None, check=True):
    child_env = dict(os.environ if env is None else env)
    program = Path(argv[0]).name
    if program == 'git':
        child_env['GIT_LITERAL_PATHSPECS'] = '1'
    if program != 'molt':
        child_env.pop('ANTHROPIC_API_KEY', None)
        child_env.pop('OPENAI_API_KEY', None)
    if program != 'gh' and not (program == 'git' and 'push' in argv[1:]):
        child_env.pop('GH_TOKEN', None)
        child_env.pop('GITHUB_TOKEN', None)
        child_env.pop('ACTIONS_RUNTIME_TOKEN', None)
        child_env.pop('ACTIONS_ID_TOKEN_REQUEST_TOKEN', None)
    p = subprocess.Popen(argv, cwd=cwd, env=child_env, stdin=subprocess.PIPE if input is not None else subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        out, err = p.communicate(input, timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGTERM)
        try:
            p.communicate(timeout=15)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            p.communicate()
        raise Failure('command timed out; its process group was stopped')
    if len(out) > 64 * 1024 * 1024 or len(err) > 64 * 1024 * 1024:
        raise Failure('command output exceeded 64 MiB')
    if check and p.returncode:
        # Do not copy process stderr into a public comment: it can contain credentials.
        raise Failure(f'{Path(argv[0]).name} exited {p.returncode}; inspect the private job logs')
    return p.returncode, out, err


def gh(*args):
    return run(['gh', *args])[1]


def eligible(event, event_name, repo, label):
    issue = event.get('issue', {})
    return (event_name == 'issues' and event.get('action') == 'labeled'
            and event.get('label', {}).get('name') == label and 'pull_request' not in issue
            and issue.get('state') == 'open' and event.get('repository', {}).get('full_name') == repo)


def safe_path(name):
    if not isinstance(name, str) or not name or '\x00' in name or '\\' in name:
        raise Failure('invalid changed path')
    p = PurePosixPath(name)
    if p.is_absolute() or any(part in ('.', '..', '.git') for part in p.parts) or str(p) != name:
        raise Failure('changed path escaped the workspace')
    return name


def trusted_binaries(bin_dir, workspace):
    for name in ('molt', 'molt-ci-check', 'molt-gateway', 'molt-tools', 'molt-memory', 'molt-planner'):
        executable = (bin_dir / name).resolve(strict=True)
        if not executable.is_file() or not os.access(executable, os.X_OK) or executable == workspace or workspace in executable.parents:
            raise Failure('Molt binaries must be executable trusted files outside the workspace')


def evidence(result, baseline):
    cost = result.get('cost_usd')
    missing = result.get('uncounted_calls', 0)
    if not isinstance(cost, (int, float)) or not math.isfinite(cost):
        spending = 'unknown'
    else:
        spending = f'${cost:.6f}' + (f' known subtotal; {missing} calls uncounted' if missing else '')
    if baseline.get('timed_out') or baseline.get('exit_code') is None:
        before = 'unknown (timed out or terminated)'
    else:
        before = f"exit {baseline['exit_code']}"
    check = result['check']['command']
    # Indent arbitrary command text instead of letting it close a Markdown fence.
    return (f"Run: `{result['run']}`\n\nCheck:\n\n" + '\n'.join('    ' + l for l in check.splitlines())
            + f"\n\nBaseline with the protected check files: {before}.\n"
              "Winning attempt: passed; the default review approved application.\n"
            + f"Cost: {spending}. Learning is disabled for CI.\n")


def main():
    event = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text())
    repo = os.environ['GITHUB_REPOSITORY']
    label = os.environ.get('MOLT_CI_LABEL', 'molt-run')
    if not eligible(event, os.environ.get('GITHUB_EVENT_NAME'), repo, label):
        raise Failure('only an open issues/labeled event may start Molt; PR events are refused')
    issue = event['issue']['number']
    if not isinstance(issue, int) or issue < 1:
        raise Failure('invalid issue number')
    actor = event['sender']['login']
    permission = json.loads(gh('api', f'repos/{repo}/collaborators/{actor}/permission')).get('permission')
    if permission not in ('admin', 'maintain', 'write'):
        raise Failure('the actor applying the label must have write permission')
    workspace = Path(os.environ['GITHUB_WORKSPACE']).resolve()
    if run(['git', 'status', '--porcelain'], cwd=workspace)[1]:
        raise Failure('the checkout must be clean before running Molt')
    base = run(['git', 'rev-parse', 'HEAD'], cwd=workspace)[1].decode().strip()
    default_branch = event['repository']['default_branch']
    remote_base = run(['git', 'rev-parse', f'origin/{default_branch}'], cwd=workspace)[1].decode().strip()
    if base != remote_base:
        raise Failure('the checkout must be the trusted default branch')
    bin_dir = Path(os.environ['MOLT_CI_BIN']).resolve()
    if bin_dir == workspace or workspace in bin_dir.parents:
        raise Failure('install Molt outside the workspace before running the action')
    trusted_binaries(bin_dir, workspace)
    budget = float(os.environ['MOLT_CI_BUDGET'])
    attempts = int(os.environ.get('MOLT_CI_ATTEMPTS', '2'))
    timeout = int(os.environ.get('MOLT_CI_TIMEOUT', '1200'))
    if not math.isfinite(budget) or budget <= 0 or not 1 <= attempts <= 8 or not 1 <= timeout <= 21600:
        raise Failure('invalid budget, attempts or timeout')
    body = str(event['issue'].get('body') or '')
    title = str(event['issue']['title'])
    task = f'Issue #{issue}: {title}\n\n{body}'
    if len(task.encode()) > 65536:
        raise Failure('issue exceeds 64 KiB')
    runner_temp = Path(os.environ['RUNNER_TEMP']).resolve()
    branch = f'molt/issue-{issue}-{os.environ["GITHUB_RUN_ID"]}-{os.environ.get("GITHUB_RUN_ATTEMPT", "1")}'
    _, names, _ = run(['git', 'config', '--name-only', '--get-regexp', r'^filter\.'], cwd=workspace, check=False)
    filters = {name.rsplit('.', 1)[0] for name in names.decode().splitlines()}
    git_prefix = ['git', '-c', 'core.hooksPath=/dev/null', '-c', 'core.fsmonitor=false']
    for name in sorted(filters):
        git_prefix.extend(['-c', name + '.clean=/bin/cat', '-c', name + '.process=', '-c', name + '.required=false'])
    started = time.monotonic()
    try:
        # Use Molt's actual sandbox probe before any model spend; no --no-sandbox path.
        run([str(bin_dir / 'molt-ci-check'), 'true', '--workspace', str(workspace)], timeout=30)
        with tempfile.TemporaryDirectory(prefix='molt-ci-', dir=runner_temp) as state:
            argv = [str(bin_dir / 'molt'), 'do', task, '--workspace', str(workspace), '--data-dir', state,
                    '--budget-usd', str(budget), '--attempts', str(attempts), '--no-learn', '--json']
            code, output, _ = run(argv, timeout=timeout, check=False)
            # Keep the full report and the audited trace in private job storage.
            report = runner_temp / 'molt-ci-result.json'
            report.write_bytes(output)
            report.chmod(0o600)
            audit = Path(state) / 'audit.jsonl'
            if audit.exists():
                shutil.copyfile(audit, runner_temp / 'molt-ci-audit.jsonl')
                (runner_temp / 'molt-ci-audit.jsonl').chmod(0o600)
            if code:
                raise Failure(f'Molt stopped with exit {code} (budget, acceptance, review or runtime failure)')
            result = json.loads(output)
            if result.get('outcome') != 'passed' or not result.get('applied') or not result.get('check'):
                raise Failure('Molt did not apply a verified result')
            if not re.fullmatch(r'[A-Za-z0-9_-]{1,128}', result.get('run', '')):
                raise Failure('Molt did not return a valid run ID')
            paths = [safe_path(c['path']) for c in result['changes']]
            if not paths:
                raise Failure('the verified result changed no files; no PR was created')
            if run([*git_prefix, 'rev-parse', 'HEAD'], cwd=workspace)[1].decode().strip() != base:
                raise Failure('checkout HEAD changed during the run')
            with tempfile.TemporaryDirectory(prefix='molt-baseline-', dir=runner_temp) as baseline_dir:
                # git archive supplies only the trusted pre-run tree, never current candidate code.
                archive = run([*git_prefix, 'archive', base], cwd=workspace)[1]
                import io
                import tarfile
                with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
                    tar.extractall(baseline_dir, filter='data')
                for name in result['check'].get('files', []):
                    name = safe_path(name)
                    src, dst = workspace / name, Path(baseline_dir) / name
                    if src.is_symlink() or not src.is_file() or workspace not in src.resolve().parents:
                        raise Failure('protected check file must be a regular workspace file')
                    if Path(baseline_dir).resolve() not in dst.resolve().parents:
                        raise Failure('baseline check path escaped')
                    dst.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(src, dst)
                baseline = json.loads(run([str(bin_dir / 'molt-ci-check'), result['check']['command'],
                                          '--workspace', baseline_dir], timeout=150)[1])
            (runner_temp / 'molt-ci-baseline.json').write_text(json.dumps(baseline))
            pr_body = (f'Closes #{issue}\n\nMolt applied a checked result for this issue.\n\n'
                       + evidence(result, baseline) + f'Wall time: {time.monotonic() - started:.1f}s.\nBaseline check duration: {baseline.get("duration_ms", "unknown")} ms.\n')
            # A separate index commits exactly Molt's reported paths, never runner artifacts.
            with tempfile.TemporaryDirectory(prefix='molt-index-', dir=runner_temp) as index_dir:
                git_env = dict(os.environ, GIT_INDEX_FILE=str(Path(index_dir) / 'index'))
                run([*git_prefix, 'read-tree', base], cwd=workspace, env=git_env)
                # Disable configured filters for all plumbing calls, not only add.
                run([*git_prefix, 'add', '-A', '--', *paths], cwd=workspace, env=git_env)
                tree = run([*git_prefix, 'write-tree'], cwd=workspace, env=git_env)[1].decode().strip()
                message = f'Fix issue #{issue}\n\nMolt-Run: {result["run"]}\nMolt-Check: {result["check"]["command"]}\n'
                commit = run([*git_prefix, '-c', 'user.name=Molt CI', '-c', 'user.email=molt-ci@users.noreply.github.com',
                              'commit-tree', tree, '-p', base], cwd=workspace, input=message.encode())[1].decode().strip()
                run([*git_prefix, 'update-ref', f'refs/heads/{branch}', commit, '0' * 40], cwd=workspace)
                run([*git_prefix, 'push', 'origin', f'{branch}:{branch}'], cwd=workspace)
            body_file = runner_temp / 'molt-ci-pr.md'
            body_file.write_text(pr_body)
            print(gh('pr', 'create', '--repo', repo, '--base', default_branch, '--head', branch,
                     '--title', f'Fix issue #{issue}', '--body-file', str(body_file)).decode().strip())
    except (Failure, ValueError, KeyError, OSError, TypeError) as error:
        comment = runner_temp / 'molt-ci-failure.md'
        comment.write_text(f'Molt CI could not create a PR for issue #{issue}.\n\n{error}\n\n'
                           f'Job: https://github.com/{repo}/actions/runs/{os.environ["GITHUB_RUN_ID"]}\n')
        gh('issue', 'comment', str(issue), '--repo', repo, '--body-file', str(comment))
        raise


if __name__ == '__main__':
    try:
        main()
    except (Failure, ValueError, KeyError, OSError, TypeError) as error:
        print(f'Molt CI: {error}', file=sys.stderr)
        sys.exit(1)

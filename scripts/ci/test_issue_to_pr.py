import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('ci', Path(__file__).with_name('issue_to_pr.py'))
ci = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ci)


class Tests(unittest.TestCase):
    def test_only_labelled_open_issues_are_eligible(self):
        event = {'action': 'labeled', 'label': {'name': 'molt-run'}, 'issue': {'state': 'open'},
                 'repository': {'full_name': 'a/b'}}
        self.assertTrue(ci.eligible(event, 'issues', 'a/b', 'molt-run'))
        self.assertFalse(ci.eligible(event, 'pull_request_target', 'a/b', 'molt-run'))
        self.assertFalse(ci.eligible(event, 'pull_request', 'a/b', 'molt-run'))
        self.assertFalse(ci.eligible(event, 'issues', 'a/c', 'molt-run'))
        event['issue']['pull_request'] = {}
        self.assertFalse(ci.eligible(event, 'issues', 'a/b', 'molt-run'))

    def test_paths_cannot_stage_files_outside_the_workspace(self):
        for path in ('../x', '/tmp/x', '.git/config', 'a/../b', 'a//b', 'a\\b', ''):
            with self.assertRaises(ci.Failure):
                ci.safe_path(path)
        self.assertEqual(ci.safe_path('src/a b.rs'), 'src/a b.rs')

    def test_unknown_cost_and_baseline_are_never_zero(self):
        result = {'run': 'trace_1', 'check': {'command': 'test\n```'}, 'uncounted_calls': 2}
        body = ci.evidence(result, {'exit_code': None, 'timed_out': True})
        self.assertIn('unknown', body)
        self.assertIn('    ```', body)
        self.assertNotIn('$0.00', body)
        result['cost_usd'] = 1.5
        self.assertIn('known subtotal; 2 calls uncounted', ci.evidence(result, {'exit_code': 1}))

    def test_timeout_kills_the_process_and_no_stderr_is_disclosed(self):
        import sys
        with self.assertRaises(ci.Failure):
            ci.run([sys.executable, '-c', 'import time; time.sleep(10)'], timeout=0.02)
        with self.assertRaises(ci.Failure) as e:
            ci.run([sys.executable, '-c', 'import sys; print("secret", file=sys.stderr); sys.exit(2)'])
        self.assertNotIn('secret', str(e.exception))

    def test_sandbox_failure_posts_a_summary_and_never_starts_a_model(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)
            event = {'action': 'labeled', 'label': {'name': 'molt-run'}, 'issue': {'state': 'open', 'number': 12, 'title': 'Fix', 'body': ''},
                     'sender': {'login': 'maintainer'}, 'repository': {'full_name': 'a/b', 'default_branch': 'main'}}
            (path / 'event.json').write_text(json.dumps(event))
            (path / 'bin').mkdir()
            for name in ('molt','molt-ci-check','molt-gateway','molt-tools','molt-memory','molt-planner'):
                binary=path/'bin'/name;binary.write_text('#!/bin/sh\nexit 0\n');binary.chmod(0o700)
            env = {'GITHUB_EVENT_PATH': str(path / 'event.json'), 'GITHUB_EVENT_NAME': 'issues', 'GITHUB_REPOSITORY': 'a/b',
                   'GITHUB_WORKSPACE': str(path / 'work'), 'RUNNER_TEMP': temp, 'GITHUB_RUN_ID': '123',
                   'MOLT_CI_BIN': str(path / 'bin'), 'MOLT_CI_BUDGET': '1'}
            calls = []
            def fake(argv, **kwargs):
                calls.append(argv)
                if argv[0] == 'git':
                    return (0, b'' if argv[1] in ('status','config') else b'abcd\n', b'')
                raise ci.Failure('sandbox unavailable')
            with patch.dict(ci.os.environ, env), patch.object(ci, 'run', fake), patch.object(ci, 'gh', return_value=b'{"permission":"write"}') as gh:
                with self.assertRaises(ci.Failure):
                    ci.main()
                self.assertEqual(gh.call_args[0][:2], ('issue', 'comment'))
            self.assertFalse(any('do' in call for call in calls))
            self.assertIn('sandbox unavailable', (path / 'molt-ci-failure.md').read_text())

    def test_success_commits_only_reported_paths_and_describes_baseline(self):
        import os
        import subprocess
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp); work=root/'work'; work.mkdir(); bare=root/'remote.git'
            git=lambda *args: subprocess.check_output(['git',*args],cwd=work,stderr=subprocess.DEVNULL)
            git('init','-b','main'); git('config','user.name','Test'); git('config','user.email','test@example.com')
            (work/'README.md').write_text('before')
            git('add','README.md'); git('commit','-m','Initial')
            subprocess.check_call(['git','init','--bare',str(bare)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            git('remote','add','origin',str(bare)); git('push','-u','origin','main')
            base=git('rev-parse','HEAD').decode().strip()
            git('config','filter.danger.clean',f'touch {root / "filter-executed"}; /bin/cat')
            git('config','filter.danger.required','true')
            event={'action':'labeled','label':{'name':'molt-run'},'issue':{'state':'open','number':12,'title':'Fix','body':'Write greeting'},'sender':{'login':'maintainer'},'repository':{'full_name':'a/b','default_branch':'main'}}
            (root/'event.json').write_text(json.dumps(event))
            (root/'bin').mkdir()
            for name in ('molt','molt-ci-check','molt-gateway','molt-tools','molt-memory','molt-planner'):
                binary=root/'bin'/name;binary.write_text('#!/bin/sh\nexit 0\n');binary.chmod(0o700)
            env={'GITHUB_EVENT_PATH':str(root/'event.json'),'GITHUB_EVENT_NAME':'issues','GITHUB_REPOSITORY':'a/b','GITHUB_WORKSPACE':str(work),'RUNNER_TEMP':temp,'GITHUB_RUN_ID':'123','GITHUB_RUN_ATTEMPT':'1','MOLT_CI_BIN':str(root/'bin'),'MOLT_CI_BUDGET':'1'}
            actual_run=ci.run
            def fake_run(argv,**kwargs):
                if Path(argv[0]).name=='molt-ci-check':
                    return 0,json.dumps({'exit_code':0 if argv[1]=='true' else 1,'timed_out':False}).encode(),b''
                if Path(argv[0]).name=='molt':
                    (work/'greeting.txt').write_text('hello')
                    (work/'private-artifact.txt').write_text('do not commit')
                    (work/'.gitattributes').write_text('greeting.txt filter=danger\n')
                    result={'run':'trace_123','outcome':'passed','applied':True,'check':{'command':'test -f greeting.txt','files':[]},'changes':[{'path':'greeting.txt'}],'cost_usd':0.12,'uncounted_calls':0}
                    return 0,json.dumps(result).encode(),b''
                return actual_run(argv,**kwargs)
            def fake_gh(*args):
                if args[0]=='api': return b'{"permission":"write"}'
                if args[:2]==('pr','create'): return b'https://github.com/a/b/pull/1'
                raise AssertionError(args)
            with patch.dict(ci.os.environ,env),patch.object(ci,'run',fake_run),patch.object(ci,'gh',fake_gh):
                ci.main()
            branch='molt/issue-12-123-1'
            files=git('ls-tree','-r','--name-only',branch).decode().splitlines()
            self.assertEqual(files,['README.md','greeting.txt'])
            self.assertFalse((root/'filter-executed').exists(), 'candidate attributes cannot run a host filter')
            self.assertEqual(git('rev-parse',branch+'^').decode().strip(),base)
            body=(root/'molt-ci-pr.md').read_text()
            self.assertIn('Closes #12',body); self.assertIn('trace_123',body); self.assertIn('exit 1',body); self.assertIn('$0.120000',body)
            self.assertIn('Molt-Run: trace_123',git('log','-1','--format=%B',branch).decode())
            self.assertEqual(git('diff','--cached','--name-only'),b'')

    def test_budget_failure_comments_without_pushing(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);event={'action':'labeled','label':{'name':'molt-run'},'issue':{'state':'open','number':12,'title':'Fix','body':''},'sender':{'login':'maintainer'},'repository':{'full_name':'a/b','default_branch':'main'}}
            (root/'event.json').write_text(json.dumps(event))
            (root/'bin').mkdir()
            for name in ('molt','molt-ci-check','molt-gateway','molt-tools','molt-memory','molt-planner'):
                binary=root/'bin'/name;binary.write_text('#!/bin/sh\nexit 0\n');binary.chmod(0o700)
            env={'GITHUB_EVENT_PATH':str(root/'event.json'),'GITHUB_EVENT_NAME':'issues','GITHUB_REPOSITORY':'a/b','GITHUB_WORKSPACE':str(root/'work'),'RUNNER_TEMP':temp,'GITHUB_RUN_ID':'123','MOLT_CI_BIN':str(root/'bin'),'MOLT_CI_BUDGET':'1'}
            calls=[]
            def fake(argv,**kwargs):
                calls.append(argv)
                if argv[0]=='git': return 0,b'' if argv[1] in ('status','config') else b'abcd\n',b''
                if Path(argv[0]).name=='molt-ci-check': return 0,b'{"exit_code":0}',b''
                return 1,b'{"outcome":"failed","summary":"budget exhausted"}',b''
            with patch.dict(ci.os.environ,env),patch.object(ci,'run',fake),patch.object(ci,'gh',return_value=b'{"permission":"write"}') as gh:
                with self.assertRaises(ci.Failure): ci.main()
                self.assertEqual(gh.call_args[0][:2],('issue','comment'))
            self.assertFalse(any('push' in call for call in calls))
            self.assertIn('Molt stopped with exit 1',(root/'molt-ci-failure.md').read_text())

    def test_model_keys_are_not_inherited_by_git_or_baseline_helpers(self):
        import os
        import sys
        with patch.dict(ci.os.environ,{'ANTHROPIC_API_KEY':'fixture-one','OPENAI_API_KEY':'fixture-two'}):
            output=ci.run([sys.executable,'-c','import os; print("ANTHROPIC_API_KEY" in os.environ, "OPENAI_API_KEY" in os.environ)'])[1]
            self.assertEqual(output.strip(),b'False False')

    def test_installed_binaries_cannot_resolve_back_into_the_workspace(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);work=root/'work';work.mkdir();binary=work/'molt';binary.write_text('fixture');binary.chmod(0o700)
            bin_dir=root/'bin';bin_dir.mkdir();(bin_dir/'molt').symlink_to(binary)
            with self.assertRaises(ci.Failure):ci.trusted_binaries(bin_dir,work)


if __name__ == '__main__':
    unittest.main()

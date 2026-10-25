import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1] / 'scripts'))
import glide_network as g
import glide_clipboard as clip

_spec = importlib.util.spec_from_file_location(
    'glide_control', Path(__file__).parents[1] / 'scripts' / 'glide-control.py')
gc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(gc)


def node(name, digit, x, y, ip):
    fp = ':'.join([digit * 2] * 32)
    return {'id': fp.replace(':', '')[:32], 'name': name, 'user': 'pi', 'ip': ip,
            'fingerprint': fp, 'x': x, 'y': y, 'active': True}


VIC = node('vic', 'a', 0, 0, '10.0.0.10')
GUS = node('gus', 'b', -1, 0, '10.0.0.11')
THIRD = node('third', 'c', 1, 0, '10.0.0.12')
LINKS = [{'name': 'eth0', 'ip': '10.0.0.10', 'network': '10.0.0.0/24'}]
LAYOUT = {'version': 1, 'machines': [VIC, GUS]}
LAYOUT3 = {'version': 1, 'machines': [VIC, GUS, THIRD]}


class Result:
    def __init__(self, stdout='', returncode=0, stderr=''):
        self.stdout, self.returncode, self.stderr = stdout, returncode, stderr


class Sysctl:
    """Fake g.run answering systemctl queries and recording every call."""

    def __init__(self, active):
        self.active = active
        self.calls = []

    def __call__(self, argv, **kw):
        self.calls.append(list(argv))
        if 'is-active' in argv:
            return Result(returncode=0 if self.active else 1)
        return Result()

    def restarts(self):
        return [c for c in self.calls if 'restart' in c]

    def starts(self):
        return [c for c in self.calls if 'start' in c]


class FakeRemote:
    """Scripted stand-in for clip.pair_remote modelling a peer machine."""

    def __init__(self, peer, layout, active=False, fail_apply=None):
        self.peer, self.layout = peer, layout
        self.state = {'enabled': False, 'peer_id': None}
        self.snapshot = {'clipboard': None, 'config': 'port = 4242\n', 'active': active}
        self.fail_apply = fail_apply
        self.calls = []

    def __call__(self, peer, action, payload=None, timeout=5):
        assert peer['id'] == self.peer['id']
        self.calls.append((action, payload))
        if action == 'probe':
            enabled = self.state['enabled']
            return {'enabled': enabled, 'peer_id': self.state['peer_id'],
                    'helper': True, 'deps': True,
                    'hook_ok': enabled,
                    'hook_expected': 'h' if enabled else None,
                    'hook_actual': 'h' if enabled else None,
                    'identity': {'id': self.peer['id'],
                                 'fingerprint': self.peer['fingerprint'],
                                 'name': self.peer['name']},
                    'layout': clip.layout_key(self.layout)}
        if action == 'snapshot':
            return dict(self.snapshot)
        if action == 'apply':
            if self.fail_apply:
                raise clip.Failure(self.fail_apply)
            self.state = {'enabled': payload['enabled'],
                          'peer_id': payload['peer_id'] if payload['enabled'] else None}
            return True
        if action == 'restore':
            assert isinstance(payload, dict) and isinstance(payload.get('active'), bool)
            self.state = {'enabled': False, 'peer_id': None}
            return True
        raise AssertionError('unexpected remote action ' + action)

    def actions(self):
        return [a for a, _ in self.calls]

    def payload(self, action):
        return dict(self.calls)[action]


class RenamedPathTests(unittest.TestCase):
    def test_installed_plugin_paths_use_wundrellama(self):
        for value in (g.REMOTE, str(g.PLUGIN), clip.REMOTE, gc.PLUGIN_SCRIPT):
            self.assertIn('wundrellama.glide', value)
            self.assertNotIn('nixfred', value)


class PairCase(unittest.TestCase):
    layout = LAYOUT
    active = True

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.config = Path(self.tmp.name) / 'config'
        self.config.mkdir()
        (self.config / 'layout.json').write_text(json.dumps(self.layout))
        self.lan = Path(self.tmp.name) / 'lan-mouse.toml'
        self.plugin = Path(self.tmp.name) / 'plugin'
        helper = self.plugin / 'scripts' / 'glide_clipboard.py'
        helper.parent.mkdir(parents=True)
        helper.write_text('# helper\n')
        self.sysctl = Sysctl(self.active)
        patches = [patch.object(g, 'CONFIG', self.config),
                   patch.object(g, 'LAN_CONFIG', self.lan),
                   patch.object(g, 'PLUGIN', self.plugin),
                   patch.object(g, 'identity', return_value=dict(VIC)),
                   patch.object(g, 'interfaces', return_value=LINKS),
                   patch.object(g, 'run', self.sysctl),
                   patch.object(clip.shutil, 'which',
                                side_effect=lambda name: '/usr/bin/' + name)]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)

    def prefs(self):
        return json.loads((self.config / 'clipboard.json').read_text())

    def clients(self):
        return g.tomllib.loads(self.lan.read_text()).get('clients', [])


class PairEnableTests(PairCase):
    def test_pair_enable_success_stages_snapshots_then_applies(self):
        remote = FakeRemote(GUS, self.layout, active=True)
        with patch.object(clip, 'pair_remote', remote):
            result = clip.pair_enable(GUS['id'])
        self.assertEqual(remote.actions(), ['probe', 'snapshot', 'apply', 'probe'])
        self.assertEqual(remote.payload('apply'), {'enabled': True, 'peer_id': VIC['id']})
        self.assertEqual(self.prefs(), {'enabled': True, 'peer_id': GUS['id']})
        hook = self.clients()[0]['enter_hook']
        self.assertTrue(hook.endswith(' send ' + GUS['id']))
        self.assertIn('glide_clipboard.py', hook)
        self.assertEqual(len(self.sysctl.restarts()), 1)  # active service restarted
        self.assertTrue(result['enabled'])
        self.assertTrue(result['ready'])
        self.assertEqual(result['state'], 'ready')
        self.assertEqual(result['peer_id'], GUS['id'])

    def test_pair_enable_rejects_unknown_or_non_adjacent_peer(self):
        for bad in ['f' * 32, 'not-an-id']:
            with self.subTest(bad=bad), self.assertRaises(clip.Failure):
                clip.pair_enable(bad)

    def test_local_failure_rolls_back_remote_and_local_bytes(self):
        original = {'enabled': False}
        (self.config / 'clipboard.json').write_text(json.dumps(original))
        self.lan.write_text('port = 4242\n')
        remote = FakeRemote(GUS, self.layout)
        with patch.object(clip, 'pair_remote', remote), \
                patch.object(g, 'render_config', side_effect=ValueError('render exploded')):
            with self.assertRaises(clip.Failure) as ctx:
                clip.pair_enable(GUS['id'])
        self.assertIn('prior state restored', str(ctx.exception))
        self.assertEqual(remote.actions(), ['probe', 'snapshot', 'apply', 'restore'])
        self.assertEqual(remote.payload('restore'), remote.snapshot)
        self.assertEqual(json.loads((self.config / 'clipboard.json').read_text()), original)
        self.assertEqual(self.lan.read_text(), 'port = 4242\n')
        self.assertEqual(self.sysctl.starts(), [])
        self.assertEqual(self.sysctl.restarts(), [])  # only try-restart, if anything

    def test_remote_apply_failure_leaves_local_untouched_and_restores_remote(self):
        (self.config / 'clipboard.json').write_text(json.dumps({'enabled': False}))
        remote = FakeRemote(GUS, self.layout, fail_apply='simulated connection loss')
        with patch.object(clip, 'pair_remote', remote):
            with self.assertRaises(clip.Failure) as ctx:
                clip.pair_enable(GUS['id'])
        self.assertIn('simulated connection loss', str(ctx.exception))
        self.assertEqual(remote.actions(), ['probe', 'snapshot', 'apply', 'restore'])
        self.assertEqual(json.loads((self.config / 'clipboard.json').read_text()),
                         {'enabled': False})
        self.assertFalse(self.lan.exists())
        self.assertEqual(self.sysctl.starts(), [])


class PairPausedTests(PairCase):
    active = False

    def test_pair_enable_never_wakes_a_paused_service(self):
        remote = FakeRemote(GUS, self.layout, active=False)
        with patch.object(clip, 'pair_remote', remote):
            result = clip.pair_enable(GUS['id'])
        self.assertEqual(self.sysctl.restarts(), [])
        self.assertEqual(self.sysctl.starts(), [])
        self.assertEqual(self.prefs(), {'enabled': True, 'peer_id': GUS['id']})
        self.assertEqual(result['state'], 'ready')

    def test_remote_failure_with_paused_services_changes_nothing(self):
        (self.config / 'clipboard.json').write_text(json.dumps({'enabled': False}))
        remote = FakeRemote(GUS, self.layout, fail_apply='simulated connection loss')
        with patch.object(clip, 'pair_remote', remote):
            with self.assertRaises(clip.Failure):
                clip.pair_enable(GUS['id'])
        self.assertEqual(self.sysctl.restarts(), [])
        self.assertEqual(self.sysctl.starts(), [])
        self.assertEqual(json.loads((self.config / 'clipboard.json').read_text()),
                         {'enabled': False})


class PairDisableTests(PairCase):
    def setUp(self):
        super().setUp()
        (self.config / 'clipboard.json').write_text(
            json.dumps({'enabled': True, 'peer_id': GUS['id']}))

    def test_pair_disable_success_clears_both_sides(self):
        remote = FakeRemote(GUS, self.layout)
        remote.state = {'enabled': True, 'peer_id': VIC['id']}
        with patch.object(clip, 'pair_remote', remote):
            result = clip.pair_disable(GUS['id'])
        self.assertEqual(remote.payload('apply'), {'enabled': False, 'peer_id': VIC['id']})
        self.assertEqual(self.prefs(), {'enabled': False})
        self.assertNotIn('enter_hook', self.clients()[0])
        self.assertFalse(result['enabled'])
        self.assertEqual(result['state'], 'off')

    def test_pair_disable_refuses_a_different_configured_peer(self):
        (self.config / 'layout.json').write_text(json.dumps(LAYOUT3))
        remote = FakeRemote(THIRD, LAYOUT3)
        with patch.object(clip, 'pair_remote', remote):
            with self.assertRaises(clip.Failure) as ctx:
                clip.pair_disable(THIRD['id'])
        self.assertIn('different peer', str(ctx.exception))
        self.assertEqual(remote.calls, [])
        self.assertEqual(self.prefs(), {'enabled': True, 'peer_id': GUS['id']})

    def test_pair_disable_requires_an_adjacent_peer(self):
        with self.assertRaises(clip.Failure):
            clip.pair_disable('f' * 32)

    def test_remote_apply_refuses_to_clear_a_different_peers_pairing(self):
        (self.config / 'layout.json').write_text(json.dumps(LAYOUT3))
        with self.assertRaises(clip.Failure) as ctx:
            clip.pair_apply_settings(THIRD['id'], False)
        self.assertIn('different peer', str(ctx.exception))
        self.assertEqual(self.prefs(), {'enabled': True, 'peer_id': GUS['id']})


class PairApplyTests(PairCase):
    def test_apply_writes_prefs_config_and_restarts_only_if_active(self):
        clip.pair_apply_settings(GUS['id'], True)
        self.assertEqual(self.prefs(), {'enabled': True, 'peer_id': GUS['id']})
        self.assertTrue(self.clients()[0]['enter_hook'].endswith(' send ' + GUS['id']))
        self.assertEqual(len(self.sysctl.restarts()), 1)

    def test_restore_recovers_bytes_and_never_wakes_paused(self):
        snap = {'clipboard': json.dumps({'enabled': True, 'peer_id': GUS['id']}),
                'config': 'port = 4242\n', 'active': False}
        (self.config / 'clipboard.json').write_text(json.dumps({'enabled': False}))
        self.lan.write_text('broken = true\n')
        clip.pair_restore(snap)
        self.assertEqual(self.prefs(), {'enabled': True, 'peer_id': GUS['id']})
        self.assertEqual(self.lan.read_text(), 'port = 4242\n')
        self.assertEqual(self.sysctl.starts(), [])
        self.assertEqual(self.sysctl.restarts(), [])
        with self.assertRaises(clip.Failure):
            clip.pair_restore({'clipboard': 'not json', 'config': None, 'active': False})
        with self.assertRaises(clip.Failure):
            clip.pair_restore('garbage')


class PairStatusTests(PairCase):
    def test_status_off_when_neither_side_opted_in(self):
        remote = FakeRemote(GUS, self.layout)
        with patch.object(clip, 'pair_remote', remote):
            result = clip.pair_status(GUS['id'])
        self.assertFalse(result['enabled'])
        self.assertFalse(result['ready'])
        self.assertEqual(result['state'], 'off')
        self.assertEqual(result['peer_id'], GUS['id'])

    def test_status_needs_attention_when_only_one_side_opted_in(self):
        clip.pair_apply_settings(GUS['id'], True)
        remote = FakeRemote(GUS, self.layout)  # remote not opted in
        with patch.object(clip, 'pair_remote', remote):
            result = clip.pair_status(GUS['id'])
        self.assertEqual(result['state'], 'needs-attention')
        self.assertFalse(result['ready'])
        self.assertIn('opted in', result['message'])

    def test_status_needs_attention_when_local_hook_missing(self):
        (self.config / 'clipboard.json').write_text(json.dumps({'enabled': False}))
        self.lan.write_text(g.render_config(LAYOUT, VIC))  # rendered without a hook
        (self.config / 'clipboard.json').write_text(
            json.dumps({'enabled': True, 'peer_id': GUS['id']}))
        remote = FakeRemote(GUS, self.layout)
        remote.state = {'enabled': True, 'peer_id': VIC['id']}
        with patch.object(clip, 'pair_remote', remote):
            result = clip.pair_status(GUS['id'])
        self.assertEqual(result['state'], 'needs-attention')
        self.assertIn('enter hook', result['message'])

    def test_status_unreachable_peer_never_raises(self):
        (self.config / 'clipboard.json').write_text(
            json.dumps({'enabled': True, 'peer_id': GUS['id']}))
        with patch.object(clip, 'pair_remote',
                          side_effect=clip.Failure('SSH authorization required')):
            result = clip.pair_status(GUS['id'])
        self.assertEqual(result['state'], 'needs-attention')
        self.assertIsNone(result['remote'])
        self.assertIn('SSH', result['message'])

    def test_status_unreachable_peer_while_off_stays_off(self):
        with patch.object(clip, 'pair_remote',
                          side_effect=clip.Failure('SSH authorization required')):
            result = clip.pair_status(GUS['id'])
        self.assertEqual(result['state'], 'off')

    def test_status_for_invalid_or_non_adjacent_peer(self):
        self.assertEqual(clip.pair_status('nope')['state'], 'off')
        result = clip.pair_status('f' * 32)
        self.assertEqual(result['state'], 'off')
        self.assertIn('adjacent', result['message'])

    def test_local_status_is_read_only_and_never_uses_ssh(self):
        with patch.object(clip, 'pair_remote',
                          side_effect=AssertionError('SSH must not run')), \
                patch.object(g, 'ssh_args',
                             side_effect=AssertionError('SSH must not run')):
            local = clip.pair_status_local(GUS['id'])
        self.assertTrue(local['valid'])
        self.assertTrue(local['adjacent'])
        self.assertEqual(local['me_id'], VIC['id'])
        self.assertFalse(local['enabled'])
        self.assertEqual(local['peer']['id'], GUS['id'])


class ReconcileTests(PairCase):
    def test_reconcile_default_disabled_is_a_noop(self):
        self.lan.write_text('port = 4242\n')
        result = clip.reconcile()
        self.assertFalse(result['changed'])
        self.assertEqual(self.lan.read_text(), 'port = 4242\n')

    def test_reconcile_installs_a_missing_hook_for_a_saved_optin(self):
        (self.config / 'clipboard.json').write_text(json.dumps({'enabled': False}))
        self.lan.write_text(g.render_config(LAYOUT, VIC))  # no hook rendered
        (self.config / 'clipboard.json').write_text(
            json.dumps({'enabled': True, 'peer_id': GUS['id']}))
        result = clip.reconcile()
        self.assertTrue(result['changed'])
        self.assertTrue(self.clients()[0]['enter_hook'].endswith(' send ' + GUS['id']))
        self.assertEqual(self.sysctl.starts(), [])
        self.assertEqual(self.sysctl.restarts(), [])  # reconcile never restarts

    def test_reconcile_is_a_noop_when_config_already_matches(self):
        (self.config / 'clipboard.json').write_text(
            json.dumps({'enabled': True, 'peer_id': GUS['id']}))
        self.lan.write_text(g.render_config(LAYOUT, VIC))
        result = clip.reconcile()
        self.assertFalse(result['changed'])

    def test_reconcile_refuses_an_invalid_opted_peer_without_guessing(self):
        self.lan.write_text('port = 4242\n')
        for bad in ['z' * 32, THIRD['id']]:  # malformed, then not in the layout
            with self.subTest(bad=bad):
                (self.config / 'clipboard.json').write_text(
                    json.dumps({'enabled': True, 'peer_id': bad}))
                with self.assertRaises(clip.Failure):
                    clip.reconcile()
        self.assertEqual(self.lan.read_text(), 'port = 4242\n')


class PairCliTests(PairCase):
    def run_cli(self, argv, stdin=''):
        out, err = io.StringIO(), io.StringIO()
        with patch.object(sys, 'stdout', out), patch.object(sys, 'stderr', err), \
                patch.object(clip.sys, 'stdin', io.StringIO(stdin)):
            code = clip.main(argv)
        return code, out.getvalue(), err.getvalue()

    def test_pair_status_cli_returns_result_json(self):
        remote = FakeRemote(GUS, self.layout)
        with patch.object(clip, 'pair_remote', remote):
            code, out, _ = self.run_cli(['pair-status', GUS['id']])
        payload = json.loads(out)
        self.assertEqual(code, 0)
        self.assertTrue(payload['ok'])
        self.assertEqual(payload['result']['state'], 'off')
        self.assertEqual(payload['result']['peer_id'], GUS['id'])
        for key in ('enabled', 'ready', 'state', 'message', 'local', 'remote'):
            self.assertIn(key, payload['result'])

    def test_pair_enable_cli_error_returns_error_json(self):
        code, out, _ = self.run_cli(['pair-enable', 'not-an-id'])
        payload = json.loads(out)
        self.assertEqual(code, 1)
        self.assertFalse(payload['ok'])
        self.assertIn('error', payload)

    def test_reconcile_cli_returns_result_json(self):
        code, out, _ = self.run_cli(['reconcile'])
        payload = json.loads(out)
        self.assertEqual(code, 0)
        self.assertTrue(payload['ok'])
        self.assertFalse(payload['result']['changed'])

    def test_remote_dispatch_rejects_actions_outside_the_allowlist(self):
        code, out, _ = self.run_cli(['_remote', 'delete-everything'])
        payload = json.loads(out)
        self.assertEqual(code, 1)
        self.assertFalse(payload['ok'])

    def test_remote_probe_returns_identity_layout_and_dependencies(self):
        code, out, _ = self.run_cli(['_remote', 'probe'])
        payload = json.loads(out)
        self.assertEqual(code, 0)
        self.assertTrue(payload['ok'])
        self.assertEqual(payload['result']['identity']['id'], VIC['id'])
        self.assertEqual(payload['result']['layout'], clip.layout_key(LAYOUT))
        self.assertIn('deps', payload['result'])
        self.assertIn('helper', payload['result'])

    def test_remote_apply_and_restore_roundtrip(self):
        code, out, _ = self.run_cli(
            ['_remote', 'apply'], json.dumps({'enabled': True, 'peer_id': GUS['id']}))
        self.assertTrue(json.loads(out)['ok'])
        self.assertEqual(self.prefs(), {'enabled': True, 'peer_id': GUS['id']})
        self.assertEqual(len(self.sysctl.restarts()), 1)
        code, out, _ = self.run_cli(
            ['_remote', 'restore'],
            json.dumps({'clipboard': None, 'config': None, 'active': False}))
        self.assertTrue(json.loads(out)['ok'])
        self.assertFalse((self.config / 'clipboard.json').exists())
        self.assertFalse(self.lan.exists())

    def test_remote_apply_rejects_a_malformed_payload(self):
        for bad in [{'enabled': 'yes', 'peer_id': GUS['id']},
                    {'enabled': True, 'peer_id': 'nope'}, 'string', 42]:
            with self.subTest(bad=bad):
                code, out, _ = self.run_cli(['_remote', 'apply'], json.dumps(bad))
                self.assertEqual(code, 1)
                self.assertFalse(json.loads(out)['ok'])


class ControlWiringTests(unittest.TestCase):
    def test_resume_reconciles_before_starting_and_fails_closed(self):
        calls = []

        def fake_run(argv, **kw):
            calls.append(list(argv))
            if 'reconcile' in argv:
                return Result(returncode=1,
                              stdout=json.dumps({'ok': False, 'error': 'bad opt-in'}))
            return Result()

        with patch.object(sys, 'argv', ['glide-control.py', 'resume']), \
                patch.object(gc.subprocess, 'run', side_effect=fake_run), \
                patch.object(gc, 'remote_action', return_value=(True, '')) as remote:
            with self.assertRaises(SystemExit):
                gc.main()
        self.assertFalse([c for c in calls if c[:3] == ['systemctl', '--user', 'start']])
        remote.assert_not_called()

    def test_resume_reconciles_then_starts_sharing(self):
        calls = []

        def fake_run(argv, **kw):
            calls.append(list(argv))
            return Result()

        with patch.object(sys, 'argv', ['glide-control.py', 'resume']), \
                patch.object(gc.subprocess, 'run', side_effect=fake_run), \
                patch.object(gc, 'remote_action', return_value=(True, '')) as remote:
            gc.main()
        kinds = ['reconcile' if 'reconcile' in c else 'start' for c in calls]
        self.assertEqual(kinds, ['reconcile', 'start'])
        remote.assert_called_once_with('resume')

    def test_settings_reconciles_before_starting(self):
        calls = []

        def fake_run(argv, **kw):
            calls.append(list(argv))
            return Result()

        with patch.object(sys, 'argv', ['glide-control.py', 'settings']), \
                patch.object(gc.subprocess, 'run', side_effect=fake_run), \
                patch.object(gc.subprocess, 'Popen') as popen:
            gc.main()
        self.assertIn('reconcile', calls[0])
        self.assertTrue(popen.called)

    def test_status_exposes_local_clipboard_config_without_ssh(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        home = Path(tmp.name)
        config = home / '.config' / 'omarchy-glide'
        config.mkdir(parents=True)
        with patch.object(gc.Path, 'home', return_value=home):
            self.assertEqual(gc.clipboard_status(), {'enabled': False, 'peer_id': ''})
            (config / 'clipboard.json').write_text(
                json.dumps({'enabled': True, 'peer_id': GUS['id']}))
            self.assertEqual(gc.clipboard_status(),
                             {'enabled': True, 'peer_id': GUS['id']})


if __name__ == '__main__':
    unittest.main()

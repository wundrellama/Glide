import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1] / 'scripts'))
import glide_network as g
import glide_clipboard as clip


def node(name, digit, x, y, ip):
    fp = ':'.join([digit * 2] * 32)
    return {'id': fp.replace(':', '')[:32], 'name': name, 'user': 'pi', 'ip': ip,
            'fingerprint': fp, 'x': x, 'y': y, 'active': True}


VIC = node('vic', 'a', 0, 0, '10.0.0.10')
GUS = node('gus', 'b', -1, 0, '10.0.0.11')
THIRD = node('third', 'c', -2, 0, '10.0.0.12')
LINKS = [{'name': 'eth0', 'ip': '10.0.0.10', 'network': '10.0.0.0/24'}]


class HookRenderTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.config = Path(self.tmp.name)

    def render(self):
        with patch.object(g, 'CONFIG', self.config), patch.object(g, 'interfaces', return_value=LINKS):
            return g.tomllib.loads(g.render_config({'version': 1, 'machines': [VIC, GUS]}, VIC))

    def test_default_render_has_no_enter_hook(self):
        self.assertNotIn('enter_hook', self.render()['clients'][0])

    def test_opt_in_adds_enter_hook_for_configured_adjacent_peer(self):
        (self.config / 'clipboard.json').write_text(json.dumps({'enabled': True, 'peer_id': GUS['id']}))
        hook = self.render()['clients'][0]['enter_hook']
        self.assertTrue(hook.startswith('python3 '))
        self.assertIn('glide_clipboard.py', hook)
        self.assertTrue(hook.endswith(' send ' + GUS['id']))
        self.assertNotIn('\n', hook)

    def test_hook_omitted_when_disabled(self):
        (self.config / 'clipboard.json').write_text(json.dumps({'enabled': False, 'peer_id': GUS['id']}))
        self.assertNotIn('enter_hook', self.render()['clients'][0])

    def test_hook_omitted_for_non_adjacent_or_malformed_peer(self):
        for peer_id in [THIRD['id'], 'z' * 32, '../evil']:
            with self.subTest(peer_id=peer_id):
                (self.config / 'clipboard.json').write_text(json.dumps({'enabled': True, 'peer_id': peer_id}))
                self.assertNotIn('enter_hook', self.render()['clients'][0])


class ClipboardRuleTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.config = Path(self.tmp.name)
        patcher = patch.object(g, 'CONFIG', self.config)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_peer_ids_must_be_32_lowercase_hex(self):
        self.assertTrue(clip.valid_id('a' * 32))
        for bad in ['A' * 32, 'a' * 31, 'g' * 32, '', 5, None, '../' + 'a' * 32]:
            with self.subTest(bad=bad):
                self.assertFalse(clip.valid_id(bad))

    def test_clipboard_is_disabled_by_default(self):
        self.assertIsNone(clip.enabled_peer())

    def test_enabled_peer_requires_valid_opt_in(self):
        (self.config / 'clipboard.json').write_text(json.dumps({'enabled': True, 'peer_id': GUS['id']}))
        self.assertEqual(clip.enabled_peer(), GUS['id'])
        (self.config / 'clipboard.json').write_text(json.dumps({'enabled': True, 'peer_id': 'nope'}))
        self.assertIsNone(clip.enabled_peer())
        (self.config / 'clipboard.json').write_text(json.dumps({'peer_id': GUS['id']}))
        self.assertIsNone(clip.enabled_peer())

    def test_mime_selection_prefers_utf8_then_plain(self):
        self.assertEqual(clip.pick_mime('text/plain\nimage/png\n', ''), 'text/plain')
        self.assertEqual(clip.pick_mime('image/png\ntext/plain;charset=utf-8\ntext/plain\n', ''), 'text/plain;charset=utf-8')

    def test_mime_selection_rejects_non_text_and_secret_hints(self):
        self.assertIsNone(clip.pick_mime('image/png\napplication/x-encrypted\n', ''))
        self.assertIsNone(clip.pick_mime('text/plain;charset=utf-8\n', 'sensitive'))
        self.assertIsNone(clip.pick_mime('CLIPBOARD_STATE=sensitive\ntext/plain\n', ''))
        self.assertIsNone(clip.pick_mime('x-kde-passwordManagerHint\ntext/plain\n', ''))

    def test_text_validation_rejects_nul_invalid_utf8_oversize_and_empty(self):
        self.assertEqual(clip.validate_text('héllo ✓'.encode()), 'héllo ✓')
        for bad in [b'a\x00b', b'\xff\xfe', b'x' * (clip.MAX + 1), b'']:
            with self.subTest(bad=bad[:8]), self.assertRaises(clip.Failure):
                clip.validate_text(bad)

    def test_real_reader_process_preserves_unicode_and_newlines(self):
        payload = 'héllo ✓\nsecond line\n\n'.encode()
        executable = self.config / 'wl-paste'
        executable.write_text('#!' + sys.executable + '\nimport os, sys\n'
                              'if "--list-types" in sys.argv:\n'
                              '    print("text/plain;charset=utf-8")\n'
                              'else:\n'
                              f'    os.write(1, {payload!r})\n')
        executable.chmod(0o700)
        self.assertEqual(clip.read_clipboard({'PATH': str(self.config)}), payload)

    def test_bounded_read_caps_output_and_times_out(self):
        self.assertEqual(clip.bounded_read(io.BytesIO(b'x' * 100), 10, 1), b'x' * 11)
        read_fd, write_fd = os.pipe()
        try:
            with os.fdopen(read_fd, 'rb') as reader:
                with self.assertRaises(clip.Failure):
                    clip.bounded_read(reader, 10, 0.2)
        finally:
            os.close(write_fd)


class Result:
    def __init__(self, stdout='', returncode=0):
        self.stdout, self.returncode = stdout, returncode


class PeerAndEnvTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.config = Path(self.tmp.name)
        (self.config / 'layout.json').write_text(json.dumps({'version': 1, 'machines': [VIC, GUS]}))
        patches = [patch.object(g, 'CONFIG', self.config),
                   patch.object(g, 'identity', return_value=VIC),
                   patch.object(g, 'on_lan', return_value=True),
                   patch.object(g, 'session_active', return_value=True),
                   patch.object(g, 'run', return_value=Result())]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)

    def test_require_peer_accepts_configured_adjacent_peer(self):
        me, peer = clip.require_peer(GUS['id'])
        self.assertEqual(me['id'], VIC['id'])
        self.assertEqual(peer['id'], GUS['id'])

    def test_require_peer_rejects_unknown_or_non_adjacent_ids(self):
        for bad in ['f' * 32, THIRD['id'], 'not-an-id']:
            with self.subTest(bad=bad), self.assertRaises(clip.Failure):
                clip.require_peer(bad)

    def test_require_peer_rejects_off_lan_inactive_session_and_stopped_service(self):
        with patch.object(g, 'on_lan', return_value=False), self.assertRaises(clip.Failure):
            clip.require_peer(GUS['id'])
        with patch.object(g, 'session_active', return_value=False), self.assertRaises(clip.Failure):
            clip.require_peer(GUS['id'])
        with patch.object(g, 'run', return_value=Result(returncode=1)), self.assertRaises(clip.Failure):
            clip.require_peer(GUS['id'])

    def test_graphical_env_imports_only_whitelisted_variables(self):
        listing = ('WAYLAND_DISPLAY=wayland-1\n'
                   'HYPRLAND_INSTANCE_SIGNATURE=abcdef1234\n'
                   'PATH=$(touch /tmp/pwned)\n'
                   'DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\n'
                   'garbage line\n')
        with patch.object(g, 'run', return_value=Result(stdout=listing)):
            env = clip.graphical_env()
        self.assertEqual(env, {'WAYLAND_DISPLAY': 'wayland-1',
                               'HYPRLAND_INSTANCE_SIGNATURE': 'abcdef1234',
                               'XDG_RUNTIME_DIR': f'/run/user/{os.getuid()}'})

    def test_graphical_env_fails_closed_without_wayland(self):
        with patch.object(g, 'run', return_value=Result(stdout='HYPRLAND_INSTANCE_SIGNATURE=abc\n')):
            with self.assertRaises(clip.Failure):
                clip.graphical_env()

    def test_send_lock_is_atomic_and_nonblocking(self):
        state = self.config / 'state'
        with patch.object(g, 'STATE', state):
            with clip.send_lock() as first:
                self.assertTrue(first)
                with clip.send_lock() as second:
                    self.assertFalse(second)
            with clip.send_lock() as third:
                self.assertTrue(third)


class FakeProc:
    def __init__(self, data):
        self.stdout = io.BytesIO(data)

    def poll(self):
        return 0

    def terminate(self):
        pass

    def wait(self, timeout=None):
        return 0


class FakeSubprocess:
    DEVNULL = subprocess.DEVNULL
    PIPE = subprocess.PIPE

    def __init__(self, clipboard=b'h\xc3\xa9llo \xe2\x9c\x93', types='text/plain;charset=utf-8\n'):
        self.clipboard, self.types = clipboard, types
        self.runs, self.popens = [], []

    def run(self, argv, **kw):
        self.runs.append((argv, kw))
        if '--list-types' in argv:
            return Result(stdout=self.types)
        return Result()

    def Popen(self, argv, **kw):
        self.popens.append((argv, kw))
        return FakeProc(self.clipboard)


class FlowTests(unittest.TestCase):
    ENV = {'WAYLAND_DISPLAY': 'wayland-1', 'XDG_RUNTIME_DIR': '/run/user/1000'}

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.config = Path(self.tmp.name)
        (self.config / 'layout.json').write_text(json.dumps({'version': 1, 'machines': [VIC, GUS]}))
        (self.config / 'clipboard.json').write_text(json.dumps({'enabled': True, 'peer_id': GUS['id']}))
        self.fake = FakeSubprocess()
        patches = [patch.object(g, 'CONFIG', self.config),
                   patch.object(g, 'STATE', self.config / 'state'),
                   patch.object(g, 'identity', return_value=VIC),
                   patch.object(g, 'on_lan', return_value=True),
                   patch.object(g, 'session_active', return_value=True),
                   patch.object(g, 'run', return_value=Result()),
                   patch.object(clip, 'graphical_env', return_value=dict(self.ENV)),
                   patch.object(clip, 'subprocess', self.fake)]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)

    def test_send_pushes_exact_bytes_over_ssh(self):
        self.assertEqual(clip.send(GUS['id']), 0)
        ssh = [c for c in self.fake.runs if c[0][0] == 'ssh']
        self.assertEqual(len(ssh), 1)
        argv, kw = ssh[0]
        self.assertEqual(kw['input'], b'h\xc3\xa9llo \xe2\x9c\x93')  # exact bytes, no newline added
        self.assertIs(kw['stdout'], subprocess.DEVNULL)
        self.assertIs(kw['stderr'], subprocess.DEVNULL)
        self.assertIn('BatchMode=yes', argv)
        self.assertTrue(argv[-1].endswith('glide_clipboard.py receive ' + VIC['id']))
        self.assertIn('--no-newline', self.fake.popens[0][0])

    def test_failed_clipboard_reader_never_sends_partial_text(self):
        proc = FakeProc(b'partial text')
        with patch.object(proc, 'wait', return_value=1), patch.object(self.fake, 'Popen', return_value=proc):
            self.assertEqual(clip.send(GUS['id']), 0)
        self.assertFalse([c for c in self.fake.runs if c[0][0] == 'ssh'])

    def test_cli_timeout_never_prints_payload_or_traceback(self):
        error = subprocess.TimeoutExpired('ssh', 25, output=b'PRIVATE CLIPBOARD')
        stderr = io.StringIO()
        with patch.object(clip, 'send', side_effect=error), patch.object(sys, 'stderr', stderr):
            self.assertEqual(clip.main(['send', GUS['id']]), 1)
        self.assertNotIn('PRIVATE', stderr.getvalue())
        self.assertNotIn('Traceback', stderr.getvalue())

    def test_send_skips_quietly_for_secret_or_non_text_clipboards(self):
        self.fake.types = 'image/png\n'
        self.assertEqual(clip.send(GUS['id']), 0)
        self.assertFalse([c for c in self.fake.runs if c[0][0] == 'ssh'])
        self.fake.types = 'text/plain;charset=utf-8\n'
        with patch.dict(os.environ, {'CLIPBOARD_STATE': 'sensitive'}):
            self.assertEqual(clip.send(GUS['id']), 0)
        self.assertFalse([c for c in self.fake.runs if c[0][0] == 'ssh'])

    def test_send_requires_opt_in_for_that_peer(self):
        with self.assertRaises(clip.Failure):
            clip.send(VIC['id'])
        (self.config / 'clipboard.json').write_text(json.dumps({'enabled': False, 'peer_id': GUS['id']}))
        with self.assertRaises(clip.Failure):
            clip.send(GUS['id'])

    def test_receive_sets_clipboard_with_hydrated_env_and_exact_bytes(self):
        payload = 'påste ✓'.encode()
        with patch.object(clip.sys, 'stdin', SimpleNamespace(buffer=io.BytesIO(payload))):
            self.assertEqual(clip.receive(GUS['id']), 0)
        self.assertEqual(len(self.fake.runs), 1)
        argv, kw = self.fake.runs[0]
        self.assertEqual(argv, ['wl-copy', '--type', 'text/plain;charset=utf-8'])
        self.assertEqual(kw['input'], payload)
        self.assertIs(kw['stdout'], subprocess.DEVNULL)
        self.assertIs(kw['stderr'], subprocess.DEVNULL)
        self.assertEqual(kw['env'], self.ENV)

    def test_receive_rejects_unpaired_source_and_invalid_bytes(self):
        with self.assertRaises(clip.Failure):
            clip.receive('f' * 32)
        with patch.object(clip.sys, 'stdin', SimpleNamespace(buffer=io.BytesIO(b'\xff\xfe'))):
            with self.assertRaises(clip.Failure):
                clip.receive(GUS['id'])
        self.assertFalse(self.fake.runs)


class CliTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.config = Path(self.tmp.name)
        (self.config / 'layout.json').write_text(json.dumps({'version': 1, 'machines': [VIC, GUS]}))
        patches = [patch.object(g, 'CONFIG', self.config),
                   patch.object(g, 'identity', return_value=VIC)]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)

    def test_enable_writes_opt_in_only_for_saved_adjacent_peer(self):
        message = clip.enable(GUS['id'])
        self.assertEqual(clip.enabled_peer(), GUS['id'])
        self.assertIn(GUS['name'], message)
        self.assertIn('re-apply', message.lower())
        for bad in [THIRD['id'], 'not-an-id']:
            with self.subTest(bad=bad), self.assertRaises(clip.Failure):
                clip.enable(bad)

    def test_disable_turns_off_and_status_reports_state(self):
        self.assertIn('disabled', clip.status().lower())
        clip.enable(GUS['id'])
        text = clip.status()
        self.assertIn(GUS['id'], text)
        self.assertIn(GUS['name'], text)
        clip.disable()
        self.assertIsNone(clip.enabled_peer())
        self.assertIn('disabled', clip.status().lower())

    def test_main_rejects_unknown_commands(self):
        self.assertEqual(clip.main(['bogus']), 2)
        self.assertEqual(clip.main(['send']), 2)
        self.assertEqual(clip.main(['status']), 0)


if __name__ == '__main__':
    unittest.main()

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).parents[1]
PLUGIN_ID = 'wundrellama.glide'


class PluginInstallTests(unittest.TestCase):
    def test_manifest_uses_fork_identity(self):
        manifest = json.loads((ROOT / 'manifest.json').read_text())
        self.assertEqual(manifest['id'], PLUGIN_ID)

    def test_snapshot_install_publishes_renamed_plugin_only(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            binaries = home / 'bin'
            binaries.mkdir()
            for name in ('omarchy', 'omarchy-shell'):
                executable = binaries / name
                executable.write_text('#!/bin/sh\nexit 0\n')
                executable.chmod(0o755)
            env = dict(os.environ, HOME=str(home), PATH=str(binaries) + os.pathsep + os.environ['PATH'])
            subprocess.run(['bash', str(ROOT / 'scripts/install-plugin.sh')], env=env,
                           check=True, capture_output=True, text=True, timeout=15)
            target = home / '.config/omarchy/plugins' / PLUGIN_ID
            self.assertTrue((target / 'Layout.qml').is_file())
            self.assertTrue((target / 'scripts/glide_clipboard.py').is_file())
            self.assertEqual(json.loads((target / 'manifest.json').read_text())['id'], PLUGIN_ID)
            self.assertFalse((home / '.config/omarchy/plugins/nixfred.glide').exists())

    def test_upgrade_retires_original_plugin_and_preserves_user_data(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            binaries = home / 'bin'
            binaries.mkdir()
            for name in ('omarchy', 'omarchy-shell'):
                executable = binaries / name
                executable.write_text('#!/bin/sh\nexit 0\n')
                executable.chmod(0o755)
            old = home / '.config/omarchy/plugins/nixfred.glide'
            old.mkdir(parents=True)
            (old / 'manifest.json').write_text(json.dumps({'id': 'nixfred.glide'}))
            (old / 'custom.txt').write_text('user customization')
            shell = home / '.config/omarchy/shell.json'
            shell.write_text('{"original": true}')
            config = home / '.config/omarchy-glide'
            config.mkdir()
            (config / 'clipboard.json').write_text('{"enabled": false}')
            env = dict(os.environ, HOME=str(home), PATH=str(binaries) + os.pathsep + os.environ['PATH'])
            for _ in range(2):
                subprocess.run(['bash', str(ROOT / 'scripts/install-plugin.sh')], env=env,
                               check=True, capture_output=True, text=True, timeout=15)
            self.assertFalse(old.exists())
            backups = list((home / '.local/state/omarchy/glide/plugin-backups').iterdir())
            self.assertEqual(len(backups), 1)
            self.assertEqual((backups[0] / 'nixfred.glide/custom.txt').read_text(), 'user customization')
            self.assertEqual((backups[0] / 'shell.json').read_text(), shell.read_text())
            self.assertEqual((config / 'clipboard.json').read_text(), '{"enabled": false}')

    def test_engine_startup_reconciles_saved_clipboard_preference(self):
        unit = (ROOT / 'omarchy-glide.service').read_text()
        reconcile = 'ExecStartPre=/usr/bin/python3 %h/.config/omarchy/plugins/wundrellama.glide/scripts/glide_clipboard.py reconcile'
        self.assertIn(reconcile, unit)
        self.assertLess(unit.index(reconcile), unit.index('ExecStart='))

    def test_glide_does_not_hold_idle_or_sleep(self):
        # Emulated remote input already resets compositor idle; an unused
        # machine running Glide must still lock and suspend.
        self.assertFalse((ROOT / 'omarchy-glide-idle.service').exists())
        unit = (ROOT / 'omarchy-glide.service').read_text()
        self.assertNotIn('systemd-inhibit', unit)
        self.assertNotIn('omarchy-glide-idle', unit)
        for name in ('scripts/glide-control.py', 'scripts/glide_network.py'):
            with self.subTest(name=name):
                self.assertNotIn('omarchy-glide-idle', (ROOT / name).read_text())
        for name in ('install.sh', 'remote-install.sh'):
            with self.subTest(name=name):
                text = (ROOT / name).read_text()
                retire = 'systemctl --user disable --now omarchy-glide-idle.service'
                self.assertIn(retire, text)
                self.assertNotIn('enable --now omarchy-glide.service omarchy-glide-discovery.service omarchy-glide-idle', text)
                self.assertLess(text.index(retire), text.index('systemctl --user restart omarchy-glide.service'))

    def test_active_install_paths_use_new_identity(self):
        for name in ('install.sh', 'remote-install.sh', 'uninstall.sh',
                     'scripts/install-plugin.sh', 'omarchy-glide-discovery.service', 'Layout.qml'):
            with self.subTest(name=name):
                self.assertNotIn('nixfred.glide', (ROOT / name).read_text())


if __name__ == '__main__':
    unittest.main()

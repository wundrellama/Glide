"""Exercise the real QML component with inert helper processes, not the live plugin."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).parents[1]


@unittest.skipUnless(shutil.which('quickshell'), 'Quickshell is not installed')
class PluginQmlTests(unittest.TestCase):
    def test_clipboard_toggle_applies_without_dirty_layout(self):
        shell = Path('/usr/share/omarchy/shell')
        if not shell.is_dir():
            self.skipTest('Omarchy shell QML imports are unavailable')
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            for name in ('Commons', 'Ui', 'Services', 'Modules'):
                if (shell / name).is_dir():
                    (base / name).symlink_to(shell / name, target_is_directory=True)
            plugin = base / 'plugin'
            (plugin / 'scripts').mkdir(parents=True)
            shutil.copy2(ROOT / 'Layout.qml', plugin / 'Layout.qml')
            network = '''import json
me = {'id':'a'*32, 'name':'Desktop', 'x':0, 'y':0}
peer = {'id':'b'*32, 'name':'Laptop', 'x':-1, 'y':0}
print(json.dumps({'ok':True,'result':{'self':me,'devices':[],'layout':{'machines':[me,peer]},'lan':[]}}))
'''
            control = '''import json
print(json.dumps({'running':True,'ready':True,'connected':True}))
'''
            clipboard = '''import json,pathlib,sys
state = pathlib.Path(__file__).parent/'test-enabled'
command = sys.argv[1]
if command == 'pair-enable': state.touch()
if command == 'pair-disable': state.unlink(missing_ok=True)
enabled = state.exists()
print(json.dumps({'ok':True,'result':{'peer_id':sys.argv[2], 'enabled':enabled,'ready':enabled,'state':'ready' if enabled else 'off','message':'Test pair ready' if enabled else 'Text sharing off'}}))
'''
            for name, source in [('network.py', network), ('glide-control.py', control), ('glide_clipboard.py', clipboard)]:
                (plugin / 'scripts' / name).write_text(source)
            harness = base / 'shell.qml'
            harness.write_text('''import QtQuick
import Quickshell
import "plugin" as Glide
ShellRoot {
  id: test
  property int step: 0
  property int ticks: 0
  Glide.Layout { id: panel }
  function check(condition, message) {
    if (!condition) { console.error("ASSERTION: " + message); Qt.quit(); return false }
    return true
  }
  Component.onCompleted: {
    panel.selected = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    panel.scan()
  }
  Timer {
    interval: 50; running: true; repeat: true
    onTriggered: {
      if (++test.ticks > 300) { console.error("ASSERTION: test timed out at step " + test.step); Qt.quit(); return }
      if (test.step === 0 && panel.draft.length === 2) {
        test.step = 1
      } else if (test.step === 1 && panel.clipboardLoaded && !panel.clipboardBusy) {
        if (!test.check(!panel.dirty && !panel.clipboardInfo.enabled, "initial off, unchanged layout")) return
        panel.changeClipboard(); test.step = 2
      } else if (test.step === 2 && panel.clipboardInfo.enabled && !panel.busy && !panel.clipboardBusy && panel.clipboardLoaded) {
        if (!test.check(panel.clipboardInfo.state === "ready" && !panel.dirty, "enabled without Apply layout")) return
        panel.changeClipboard(); test.step = 3
      } else if (test.step === 3 && !panel.clipboardInfo.enabled && !panel.busy && !panel.clipboardBusy && panel.clipboardLoaded) {
        if (!test.check(panel.clipboardInfo.state === "off", "pair disabled")) return
        console.info("GLIDE_QML_TEST_PASS"); Qt.quit()
      }
    }
  }
}
''')
            env = dict(os.environ, QT_QPA_PLATFORM='wayland', QT_QUICK_BACKEND='software', QML_DISABLE_DISK_CACHE='1',
                       QT_FORCE_STDERR_LOGGING='1', QT_LOGGING_RULES='*.debug=false;*.warning=true;*.critical=true')
            session = subprocess.run(['systemctl', '--user', 'show-environment'], capture_output=True, text=True, timeout=5)
            for line in session.stdout.splitlines():
                key, _, value = line.partition('=')
                if key in ('WAYLAND_DISPLAY', 'XDG_RUNTIME_DIR', 'HYPRLAND_INSTANCE_SIGNATURE'):
                    env[key] = value
            if not env.get('WAYLAND_DISPLAY'):
                self.skipTest('A live Wayland session is required by Quickshell PanelWindow')
            result = subprocess.run(['quickshell', '-p', str(harness), '--no-color'],
                                    env=env, capture_output=True, text=True, timeout=35)
            output = result.stdout + '\n' + result.stderr
            self.assertEqual(result.returncode, 0, output)
            self.assertIn('GLIDE_QML_TEST_PASS', output)
            self.assertNotIn('ASSERTION:', output)


if __name__ == '__main__':
    unittest.main()

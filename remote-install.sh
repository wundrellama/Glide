#!/usr/bin/env bash
set -euo pipefail
base="$HOME/.local/share/omarchy-glide-install"
# SSH does not load the graphical session environment. Use Omarchy's stable
# system path when OMARCHY_PATH was not exported into the noninteractive shell.
export OMARCHY_PATH="${OMARCHY_PATH:-/usr/share/omarchy}"
# Avoid Qt locale warnings contaminating the shell IPC response over SSH.
export LANG=C.UTF-8 LC_ALL=C.UTF-8
command -v omarchy >/dev/null || { echo 'Remote install requires Omarchy.' >&2; exit 1; }
printf '%s\n' 'Glide needs this Omarchy user’s sudo access to install its dependencies and Avahi.'
sudo -v || { echo 'Glide remote setup stopped: this account cannot use sudo. Ask an Omarchy administrator to install Glide locally on this machine, then pair again.' >&2; exit 1; }
omarchy pkg add python-evdev python-dbus python-gobject avahi wl-clipboard
sudo systemctl enable --now avahi-daemon.service
mkdir -p "$HOME/.local/bin" "$HOME/.config/omarchy/plugins/wundrellama.glide/scripts" "$HOME/.config/omarchy/plugins/wundrellama.glide/assets" "$HOME/.config/systemd/user"
install -m755 "$base/omarchy-glide-engine" "$HOME/.local/bin/omarchy-glide-engine"
cp "$base/Layout.qml" "$base/manifest.json" "$HOME/.config/omarchy/plugins/wundrellama.glide/"
cp "$base/assets/glide-icon.png" "$HOME/.config/omarchy/plugins/wundrellama.glide/assets/"
cp "$base"/scripts/*.py "$HOME/.config/omarchy/plugins/wundrellama.glide/scripts/"
cp "$base/remote-install.sh" "$HOME/.config/omarchy/plugins/wundrellama.glide/"
cp "$base"/omarchy-glide*.service "$HOME/.config/systemd/user/"
python3 "$HOME/.config/omarchy/plugins/wundrellama.glide/scripts/setup.py"
python3 "$HOME/.config/omarchy/plugins/wundrellama.glide/scripts/disable-screensaver-hotcorner.py"
python3 - <<'PY'
import json
from pathlib import Path
p = Path.home()/'.local/state/omarchy/glide/install.json'
p.parent.mkdir(parents=True, exist_ok=True)
p.write_text(json.dumps({'bundle': True, 'admin': True, 'units': False}))
PY
systemctl --user daemon-reload
systemctl --user disable --now omarchy-glide-owner.service >/dev/null 2>&1 || true
rm -f "$HOME/.config/systemd/user/omarchy-glide-owner.service"
# Retired: it re-disabled Omarchy idle every 20s, so the screen never locked.
# Stopping it runs its ExecStopPost, which turns Omarchy idle back on.
systemctl --user disable --now omarchy-glide-idle.service >/dev/null 2>&1 || true
rm -f "$HOME/.config/systemd/user/omarchy-glide-idle.service"
systemctl --user daemon-reload
systemctl --user enable omarchy-glide.service omarchy-glide-discovery.service
# Restart, not just start, so a running engine drops its old idle:sleep inhibitor.
systemctl --user restart omarchy-glide.service omarchy-glide-discovery.service
python3 - <<'PY'
import json
from pathlib import Path
p = Path.home()/'.local/state/omarchy/glide/install.json'
data = json.loads(p.read_text())
data['units'] = True
p.write_text(json.dumps(data))
PY
omarchy plugin validate "$HOME/.config/omarchy/plugins/wundrellama.glide" >/dev/null
python3 "$HOME/.config/omarchy/plugins/wundrellama.glide/scripts/retire-plugin.py"
omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
omarchy bar put wundrellama.glide --section right >/dev/null 2>&1 || true
rm -rf "$base"
printf '%s\n' 'Glide installed on Omarchy and ready for pairing.'

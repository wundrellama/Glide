#!/usr/bin/env bash
set -euo pipefail
base=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
omarchy plugin disable wundrellama.glide
systemctl --user disable --now omarchy-glide.service omarchy-glide-discovery.service
# Older installs also carry the retired idle unit.
systemctl --user disable --now omarchy-glide-idle.service >/dev/null 2>&1 || true
python3 - <<'PY'
import json, subprocess
from pathlib import Path
p=Path.home()/'.local/state/omarchy/glide/firewall-rules.json'
if p.exists():
    for rule in json.loads(p.read_text()):
        subprocess.run(['sudo','ufw','delete']+rule, check=True)
    p.unlink()
PY
printf '%s\n' 'Glide is disabled. Pairing identities, source and recovery files are retained.'

#!/usr/bin/env python3
"""Retire the original plugin identity without deleting its files or user data."""
import json
from pathlib import Path
import shutil
import subprocess
import tempfile


def retire():
    home = Path.home()
    old = home / '.config/omarchy/plugins/nixfred.glide'
    if not old.exists() and not old.is_symlink():
        return
    manifest = old / 'manifest.json'
    if not manifest.is_file() or json.loads(manifest.read_text()).get('id') != 'nixfred.glide':
        raise SystemExit('Refusing to retire an unrecognized original Glide plugin')
    backups = home / '.local/state/omarchy/glide/plugin-backups'
    backups.mkdir(parents=True, exist_ok=True)
    backup = Path(tempfile.mkdtemp(prefix='original-', dir=backups))
    config = home / '.config/omarchy/shell.json'
    if config.is_file():
        shutil.copy2(config, backup / 'shell.json')
    # The shell may not have inventoried this directory yet. Failure is harmless:
    # moving the old manifest outside the catalog also prevents it from loading.
    subprocess.run(['omarchy', 'plugin', 'disable', 'nixfred.glide'],
                   capture_output=True, text=True, timeout=15, check=False)
    shutil.move(str(old), str(backup / old.name))
    print(f'Original Glide plugin retained at {backup}')


if __name__ == '__main__':
    retire()

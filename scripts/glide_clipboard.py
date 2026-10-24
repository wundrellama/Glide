"""Opt-in text-on-crossing clipboard prototype for Glide.

When enabled per machine, an outbound pointer crossing pushes the local
UTF-8 text selection to the configured adjacent peer over the existing,
already-authorized SSH channel. Disabled by default; both ends must opt in
and name each other. Clipboard contents are never logged.
"""
import contextlib
import fcntl
import io
import json
import os
from pathlib import Path
import re
import select
import subprocess
from subprocess import TimeoutExpired
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import glide_network as g

MAX = 1024 * 1024  # 1 MiB hard bound on clipboard text
REMOTE = 'python3 .config/omarchy/plugins/nixfred.glide/scripts/glide_clipboard.py'
ENV_WHITELIST = ('WAYLAND_DISPLAY', 'HYPRLAND_INSTANCE_SIGNATURE')


class Failure(Exception):
    """A refusal that must never carry clipboard contents."""


def valid_id(value):
    return isinstance(value, str) and re.fullmatch(r'[0-9a-f]{32}', value) is not None


def settings():
    data = g.read_json(g.CONFIG / 'clipboard.json', {})
    return data if isinstance(data, dict) else {}


def enabled_peer():
    config = settings()
    if config.get('enabled') and valid_id(config.get('peer_id')):
        return config['peer_id']
    return None


def pick_mime(list_types_output, clipboard_state):
    """Choose a UTF-8 text MIME, or None for non-text/secret clipboards."""
    if 'sensitive' in (clipboard_state or '').lower():
        return None
    offered = {line.strip() for line in list_types_output.splitlines()}
    for line in offered:
        hint = line.lower()
        if 'sensitive' in hint or 'password' in hint:
            return None
    for wanted in ('text/plain;charset=utf-8', 'text/plain'):
        if wanted in offered:
            return wanted
    return None


def validate_text(data):
    if not data:
        raise Failure('clipboard is empty')
    if len(data) > MAX:
        raise Failure('clipboard text exceeds the 1 MiB limit')
    if b'\x00' in data:
        raise Failure('clipboard contains non-text data')
    try:
        return data.decode('utf-8')
    except UnicodeDecodeError:
        raise Failure('clipboard is not valid UTF-8 text')


def require_peer(peer_id):
    """Fail closed unless peer_id is a saved, adjacent, on-LAN peer and both
    the local session and the Glide sharing service are active."""
    if not valid_id(peer_id):
        raise Failure('invalid device identity')
    me = g.identity()
    layout = g.read_json(g.CONFIG / 'layout.json', None)
    if not isinstance(layout, dict):
        raise Failure('no saved Glide layout')
    nodes = g.validate_layout(layout)
    local = next((n for n in nodes if n['id'] == me['id']), None)
    if not local:
        raise Failure('this machine is not in the saved layout')
    peer = next((p for p in g.neighbors(local, nodes).values() if p['id'] == peer_id), None)
    if not peer:
        raise Failure('peer is not adjacent in the saved layout')
    if not g.on_lan(peer['ip']):
        raise Failure('peer is not on a directly attached LAN')
    if not g.session_active():
        raise Failure('no active, unlocked local desktop session')
    if g.run(['systemctl', '--user', 'is-active', '--quiet', g.SERVICE]).returncode:
        raise Failure('the Glide sharing service is not active')
    return me, peer


def graphical_env():
    """Import only the whitelisted graphical-session variables; no eval."""
    result = g.run(['systemctl', '--user', 'show-environment'], timeout=5)
    env = {}
    for line in result.stdout.splitlines():
        key, sep, value = line.partition('=')
        if not sep or key not in ENV_WHITELIST:
            continue
        if re.fullmatch(r'[A-Za-z0-9_./-]+', value):
            env[key] = value
    if 'WAYLAND_DISPLAY' not in env:
        raise Failure('no graphical session environment available')
    env['XDG_RUNTIME_DIR'] = f'/run/user/{os.getuid()}'
    return env


@contextlib.contextmanager
def send_lock():
    """Nonblocking inter-process lock so concurrent crossings skip quietly."""
    g.STATE.mkdir(parents=True, exist_ok=True)
    fd = os.open(g.STATE / 'clipboard-send.lock', os.O_CREAT | os.O_RDWR, 0o600)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            yield False
            return
        yield True
    finally:
        os.close(fd)


def read_clipboard(env):
    """Read the local UTF-8 text selection with a hard byte and time bound."""
    listing = subprocess.run(['wl-paste', '--list-types'], capture_output=True,
                             text=True, timeout=5, env=env)
    if listing.returncode:
        raise Failure('no clipboard selection')
    mime = pick_mime(listing.stdout, os.environ.get('CLIPBOARD_STATE', ''))
    if not mime:
        raise Failure('no shareable UTF-8 text selection')
    proc = subprocess.Popen(['wl-paste', '--no-newline', '--type', mime],
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
    assert proc.stdout is not None
    try:
        data = bounded_read(proc.stdout, MAX, 5)
        if proc.wait(timeout=2):
            raise Failure('could not read the local clipboard')
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=2)
            except TimeoutExpired:
                proc.kill()
                proc.wait(timeout=2)
        proc.stdout.close()
    return data


def send(peer_id):
    """Push the local text selection to the configured peer. Quiet on skip."""
    if enabled_peer() != peer_id:
        raise Failure('text-on-crossing is not enabled for this peer')
    me, peer = require_peer(peer_id)
    with send_lock() as acquired:
        if not acquired:
            return 0
        try:
            env = graphical_env()
            data = read_clipboard(env)
            validate_text(data)
        except Failure:
            return 0  # nothing safe to send; crossings must stay quiet
        command = g.ssh_args(peer) + [REMOTE + ' receive ' + me['id']]
        result = subprocess.run(command, input=data, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, timeout=25)
        return 0 if result.returncode == 0 else 1


def receive(source_id):
    """Install text pushed by the paired source onto the local clipboard."""
    if enabled_peer() != source_id:
        raise Failure('text-on-crossing is not enabled for this source')
    require_peer(source_id)
    data = bounded_read(sys.stdin.buffer, MAX, 15)
    validate_text(data)
    env = graphical_env()
    result = subprocess.run(['wl-copy', '--type', 'text/plain;charset=utf-8'],
                            input=data, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, timeout=10, env=env)
    if result.returncode:
        raise Failure('could not set the local clipboard')
    return 0


def adjacent_peer(peer_id):
    """The saved, validated, adjacent peer node, or None."""
    if not valid_id(peer_id):
        return None
    layout = g.read_json(g.CONFIG / 'layout.json', None)
    if not isinstance(layout, dict):
        return None
    try:
        nodes = g.validate_layout(layout)
    except (ValueError, KeyError):
        return None
    me = g.identity()
    local = next((n for n in nodes if n['id'] == me['id']), None)
    if not local:
        return None
    return next((p for p in g.neighbors(local, nodes).values() if p['id'] == peer_id), None)


def enable(peer_id):
    peer = adjacent_peer(peer_id)
    if not peer:
        raise Failure('peer must be adjacent in the saved Glide layout; run enable with a valid device identity')
    g.atomic(g.CONFIG / 'clipboard.json', json.dumps({'enabled': True, 'peer_id': peer_id}, indent=2))
    return (f"Text-on-crossing enabled towards {peer['name']} ({peer_id}). "
            'This only writes clipboard.json; re-apply the Glide layout and restart '
            'the Glide service for the enter hook to take effect. The peer must run '
            f'the matching enable with this machine’s identity.')


def disable():
    g.atomic(g.CONFIG / 'clipboard.json', json.dumps({'enabled': False}, indent=2))
    return ('Text-on-crossing disabled. Re-apply the Glide layout and restart the '
            'Glide service to remove any installed enter hook.')


def status():
    peer_id = enabled_peer()
    if not peer_id:
        return 'Text-on-crossing is disabled (the default); nothing is shared on crossing.'
    peer = adjacent_peer(peer_id)
    name = peer['name'] if peer else 'not adjacent in the saved layout!'
    hook = 'will render an enter hook' if peer and g.clipboard_hook(peer_id) else 'will NOT render an enter hook'
    return (f'Text-on-crossing enabled towards {name} ({peer_id}); current saved layout '
            f'{hook}. Direction: this machine sends its clipboard text to that peer on crossing.')


def main(argv):
    try:
        if len(argv) == 2 and argv[0] == 'send':
            return send(argv[1])
        if len(argv) == 2 and argv[0] == 'receive':
            return receive(argv[1])
        if len(argv) == 2 and argv[0] == 'enable':
            print(enable(argv[1]))
            return 0
        if argv == ['disable']:
            print(disable())
            return 0
        if argv == ['status']:
            print(status())
            return 0
        print('usage: glide_clipboard.py send <peerid> | receive <sourceid> | '
              'enable <peerid> | disable | status', file=sys.stderr)
        return 2
    except (TimeoutExpired, OSError):
        print('glide-clipboard: clipboard operation unavailable or timed out', file=sys.stderr)
        return 1
    except (Failure, ValueError) as exc:
        # Never include clipboard contents in any error path.
        print('glide-clipboard: ' + str(exc), file=sys.stderr)
        return 1


def bounded_read(stream, limit, timeout):
    """Read at most limit+1 bytes; raise Failure on timeout."""
    try:
        fd = stream.fileno()
    except (AttributeError, OSError, io.UnsupportedOperation):
        fd = None
    chunks, remaining = [], limit + 1
    deadline = time.monotonic() + timeout
    while remaining > 0:
        if fd is not None:
            left = deadline - time.monotonic()
            if left <= 0 or not select.select([fd], [], [], left)[0]:
                raise Failure('timed out reading clipboard data')
            chunk = os.read(fd, min(65536, remaining))
        else:
            chunk = stream.read(min(65536, remaining))
        if not chunk:
            break
        chunks.append(chunk)
        remaining -= len(chunk)
    return b''.join(chunks)


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))

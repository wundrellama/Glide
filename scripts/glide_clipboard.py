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
import shutil
import subprocess
from subprocess import TimeoutExpired
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import glide_network as g

MAX = 1024 * 1024  # 1 MiB hard bound on clipboard text
REMOTE = 'python3 .config/omarchy/plugins/wundrellama.glide/scripts/glide_clipboard.py'
ENV_WHITELIST = ('WAYLAND_DISPLAY', 'HYPRLAND_INSTANCE_SIGNATURE')
PAIR_ACTIONS = ('probe', 'snapshot', 'apply', 'restore')
PAIR_PAYLOAD_MAX = 1024 * 1024


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


def layout_key(layout):
    """JSON-stable comparison key for a saved layout (positions + identities)."""
    if not isinstance(layout, dict):
        return None
    try:
        nodes = g.validate_layout(layout)
    except (ValueError, KeyError):
        return None
    return sorted([[n['id'], n['x'], n['y'], n['ip'], n['fingerprint']] for n in nodes])


def actual_hook(peer):
    """The enter_hook the live engine config has for this peer, or None."""
    try:
        config = g.tomllib.loads(g.LAN_CONFIG.read_text())
    except (OSError, ValueError):
        return None
    for client in config.get('clients', []):
        if client.get('ips') == [peer['ip']]:
            return client.get('enter_hook')
    return None


def pair_status_local(peer_id):
    """Read-only local half of pair-status. Never uses SSH, never writes state."""
    out = {'peer_id': peer_id, 'valid': valid_id(peer_id), 'enabled': False,
           'adjacent': False, 'peer': None, 'me_id': None, 'layout_key': None,
           'hook_expected': None, 'hook_actual': None, 'hook_ok': False,
           'helper': (g.PLUGIN / 'scripts' / 'glide_clipboard.py').exists(),
           'deps': bool(shutil.which('wl-copy') and shutil.which('wl-paste'))}
    if not out['valid']:
        return out
    out['me_id'] = g.identity()['id']
    out['layout_key'] = layout_key(g.read_json(g.CONFIG / 'layout.json', None))
    out['enabled'] = enabled_peer() == peer_id
    peer = adjacent_peer(peer_id)
    if peer:
        out['adjacent'] = True
        out['peer'] = {'id': peer['id'], 'name': peer['name'], 'ip': peer['ip']}
        expected = g.clipboard_hook(peer_id) if out['enabled'] else None
        actual = actual_hook(peer)
        out.update(hook_expected=expected, hook_actual=actual,
                   hook_ok=(expected is not None and expected == actual)
                   if out['enabled'] else actual is None)
    return out


def local_pair_view():
    """This machine's own configured opt-in state, served to the remote probe."""
    peer_id = enabled_peer()
    view = {'enabled': peer_id is not None, 'peer_id': peer_id,
            'helper': (g.PLUGIN / 'scripts' / 'glide_clipboard.py').exists(),
            'deps': bool(shutil.which('wl-copy') and shutil.which('wl-paste')),
            'hook_expected': None, 'hook_actual': None, 'hook_ok': True}
    if peer_id:
        peer = adjacent_peer(peer_id)
        expected = g.clipboard_hook(peer_id)
        actual = actual_hook(peer) if peer else None
        view.update(hook_expected=expected, hook_actual=actual,
                    hook_ok=bool(expected) and expected == actual)
    return view


def remote_probe():
    me = g.identity()
    view = local_pair_view()
    view['identity'] = {'id': me['id'], 'fingerprint': me['fingerprint'], 'name': me['name']}
    view['layout'] = layout_key(g.read_json(g.CONFIG / 'layout.json', None))
    return view


def pair_remote(peer, action, payload=None, timeout=5):
    """Bounded, allowlisted clipboard-pairing operation on a paired peer."""
    if action not in PAIR_ACTIONS:
        raise Failure('invalid remote action')
    response = g.run(g.ssh_args(peer) + [REMOTE + ' _remote ' + action],
                     input=json.dumps(payload) if payload is not None else '',
                     timeout=timeout)
    if response.returncode:
        raise Failure('SSH authorization required or peer unavailable. '
                      + (getattr(response, 'stderr', '') or '').strip()[-300:])
    try:
        data = json.loads(response.stdout)
    except ValueError:
        raise Failure('peer is running an incompatible Glide helper')
    if not data.get('ok'):
        raise Failure(str(data.get('error', 'request failed')))
    return data['result']


def pair_snapshot():
    """Raw bytes of both managed files plus the service's active state."""
    clip_path, lan = g.CONFIG / 'clipboard.json', g.LAN_CONFIG
    return {'clipboard': clip_path.read_text() if clip_path.exists() else None,
            'config': lan.read_text() if lan.exists() else None,
            'active': g.run(['systemctl', '--user', 'is-active', '--quiet',
                             g.SERVICE]).returncode == 0}


def pair_restore(snap):
    """Roll back managed bytes; try-restart reloads only an active service."""
    if not isinstance(snap, dict) or not isinstance(snap.get('active'), bool):
        raise Failure('invalid recovery snapshot')
    clip_data = snap.get('clipboard')
    if clip_data is None:
        (g.CONFIG / 'clipboard.json').unlink(missing_ok=True)
    else:
        try:
            json.loads(clip_data)  # never restore corrupt preferences
        except (TypeError, ValueError):
            raise Failure('invalid recovery snapshot')
        g.atomic(g.CONFIG / 'clipboard.json', clip_data)
    config = snap.get('config')
    if config is None:
        g.LAN_CONFIG.unlink(missing_ok=True)
    else:
        try:
            g.tomllib.loads(config)  # never restore a corrupt engine config
        except (TypeError, ValueError):
            raise Failure('invalid recovery snapshot')
        g.atomic(g.LAN_CONFIG, config)
    # try-restart restarts an active service with the recovered bytes and
    # leaves a paused service untouched: a rollback must never wake it.
    g.run(['systemctl', '--user', 'try-restart', g.SERVICE])


def pair_apply_settings(peer_id, enable):
    """Write prefs, regenerate the engine config, restart only if active.

    Used by the local pair flow and by the peer's own '_remote apply'. Any
    failure restores the prior bytes and never wakes a paused service.
    """
    me = g.identity()
    layout = g.read_json(g.CONFIG / 'layout.json', None)
    if not isinstance(layout, dict):
        raise Failure('no saved Glide layout')
    nodes = g.validate_layout(layout)
    local = next((n for n in nodes if n['id'] == me['id']), None)
    if not local:
        raise Failure('this machine is not in the saved layout')
    if not any(p['id'] == peer_id for p in g.neighbors(local, nodes).values()):
        raise Failure('peer is not adjacent in the saved layout')
    current = settings()
    if not enable and current.get('enabled') and current.get('peer_id') != peer_id:
        raise Failure('this machine is paired with a different peer; refusing')
    snap = pair_snapshot()
    try:
        prefs = {'enabled': True, 'peer_id': peer_id} if enable else {'enabled': False}
        g.atomic(g.CONFIG / 'clipboard.json', json.dumps(prefs, indent=2))
        content = g.render_config(layout, me)  # reads the prefs just written
        g.atomic(g.LAN_CONFIG, content)
    except Exception:
        pair_restore(snap)
        raise
    if snap['active']:
        check = g.run(['systemctl', '--user', 'restart', g.SERVICE])
        if check.returncode:
            pair_restore(snap)
            raise Failure('could not restart the Glide sharing service')
    return True


def pair_precheck(peer_id):
    """Saved identity + adjacency + direct-LAN validation shared by pair ops."""
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
    return me, layout, peer


def check_probe(probe, peer, key):
    ident = probe.get('identity') or {}
    if ident.get('id') != peer['id'] or ident.get('fingerprint') != peer['fingerprint']:
        raise Failure(f"{peer['name']}: device identity changed; reauthorize it")
    if probe.get('layout') != key:
        raise Failure(f"{peer['name']}: has a different saved Glide layout; "
                      're-apply the layout on both machines first')
    if not probe.get('helper'):
        raise Failure(f"{peer['name']}: the clipboard helper is not installed")
    if not probe.get('deps'):
        raise Failure(f"{peer['name']}: wl-clipboard (wl-copy/wl-paste) is not installed")


def pair_enable(peer_id):
    """Opt both machines into text-on-crossing towards each other, atomically."""
    me, layout, peer = pair_precheck(peer_id)
    probe = pair_remote(peer, 'probe')
    check_probe(probe, peer, layout_key(layout))
    remote_snap = pair_remote(peer, 'snapshot')
    local_snap = pair_snapshot()
    try:
        pair_remote(peer, 'apply', {'enabled': True, 'peer_id': me['id']}, timeout=10)
    except Exception as exc:
        note = ''
        try:
            pair_remote(peer, 'restore', remote_snap, timeout=10)
        except Exception as recovery:
            note = '; recovery requires attention: ' + str(recovery)
        raise Failure(f"{peer['name']}: {exc}{note}")
    try:
        pair_apply_settings(peer_id, True)
    except Exception as exc:
        failures = []
        try:
            pair_remote(peer, 'restore', remote_snap, timeout=10)
        except Exception as recovery:
            failures.append(str(recovery))
        try:
            pair_restore(local_snap)
        except Exception as recovery:
            failures.append(str(recovery))
        raise Failure(str(exc) + ('; recovery requires attention: ' + '; '.join(failures)
                                  if failures else '; prior state restored on both machines'))
    return pair_status(peer_id)


def pair_disable(peer_id):
    """Opt both machines out; refuses to disturb a different configured peer."""
    me, layout, peer = pair_precheck(peer_id)
    current = settings()
    if current.get('enabled') and current.get('peer_id') != peer_id:
        raise Failure('this machine is paired with a different peer; refusing')
    probe = pair_remote(peer, 'probe')
    check_probe(probe, peer, layout_key(layout))
    remote_snap = pair_remote(peer, 'snapshot')
    local_snap = pair_snapshot()
    try:
        pair_remote(peer, 'apply', {'enabled': False, 'peer_id': me['id']}, timeout=10)
    except Exception as exc:
        note = ''
        try:
            pair_remote(peer, 'restore', remote_snap, timeout=10)
        except Exception as recovery:
            note = '; recovery requires attention: ' + str(recovery)
        raise Failure(f"{peer['name']}: {exc}{note}")
    try:
        pair_apply_settings(peer_id, False)
    except Exception as exc:
        failures = []
        try:
            pair_remote(peer, 'restore', remote_snap, timeout=10)
        except Exception as recovery:
            failures.append(str(recovery))
        try:
            pair_restore(local_snap)
        except Exception as recovery:
            failures.append(str(recovery))
        raise Failure(str(exc) + ('; recovery requires attention: ' + '; '.join(failures)
                                  if failures else '; prior state restored on both machines'))
    return pair_status(peer_id)


def pair_status(peer_id):
    """Read-only pair state for one peer. Probes the peer with a <=5s budget."""
    local = pair_status_local(peer_id)
    result = {'peer_id': peer_id, 'enabled': False, 'ready': False, 'state': 'off',
              'message': 'text-on-crossing is not enabled for this peer',
              'local': local, 'remote': None}
    if not local['valid']:
        result['message'] = 'not a valid device identity'
        return result
    if not local['adjacent']:
        if local['enabled']:
            result.update(state='needs-attention',
                          message='the configured peer is not adjacent in the saved '
                                  'layout; re-apply the layout or disable the pairing')
        else:
            result['message'] = 'peer is not adjacent in the saved Glide layout'
        return result
    peer = adjacent_peer(peer_id)
    if not peer:
        return result  # unreachable: local['adjacent'] was verified above
    try:
        remote = pair_remote(peer, 'probe')
    except Exception as exc:
        if local['enabled']:
            result.update(state='needs-attention',
                          message='peer is unreachable over BatchMode SSH: ' + str(exc))
        else:
            result['message'] = 'not enabled; peer is currently unreachable over SSH'
        return result
    result['remote'] = remote
    ident = remote.get('identity') or {}
    if ident.get('id') != peer['id'] or ident.get('fingerprint') != peer['fingerprint']:
        result.update(state='needs-attention',
                      message='peer device identity changed; reauthorize it')
        return result
    remote_enabled = bool(remote.get('enabled') and remote.get('peer_id') == local['me_id'])
    enabled = local['enabled'] and remote_enabled
    problems = []
    if local['enabled'] != remote_enabled:
        problems.append('only one side has opted in towards the other')
    if remote.get('layout') != local['layout_key']:
        problems.append('the peer has a different saved Glide layout')
    if enabled:
        if not local['hook_ok']:
            problems.append('the local engine config is missing the enter hook '
                            '(run reconcile or re-apply the layout)')
        if not remote.get('hook_ok'):
            problems.append('the peer’s engine config is missing its enter hook')
        if not local['deps'] or not remote.get('deps'):
            problems.append('wl-clipboard (wl-copy/wl-paste) is missing on one side')
        if not local['helper'] or not remote.get('helper'):
            problems.append('the installed clipboard helper is missing on one side')
    result['enabled'] = enabled
    result['ready'] = enabled and not problems
    if result['ready']:
        result['state'] = 'ready'
        result['message'] = ('both ends opted in and verified; clipboard text '
                             'follows pointer crossings')
    elif local['enabled'] or remote_enabled:
        result['state'] = 'needs-attention'
        result['message'] = '; '.join(problems)
    return result


def remote_dispatch(action, payload):
    """The peer-side handler behind '_remote'; strictly allowlisted."""
    if action == 'probe':
        return remote_probe()
    if action == 'snapshot':
        return pair_snapshot()
    if action == 'apply':
        if (not isinstance(payload, dict) or type(payload.get('enabled')) is not bool
                or not valid_id(payload.get('peer_id'))):
            raise Failure('invalid apply payload')
        return pair_apply_settings(payload['peer_id'], payload['enabled'])
    if action == 'restore':
        pair_restore(payload)
        return True
    raise Failure('invalid remote action')


def reconcile():
    """Reflect a valid saved local clipboard opt-in into the engine config.

    Rewrites the LAN config from the saved layout (atomically, only when it
    differs) so the opt-in's enter hook is present. Never starts or restarts
    anything, and refuses an invalid or non-adjacent opted peer instead of
    guessing.
    """
    current = settings()
    if not current.get('enabled'):
        return {'changed': False, 'reason': 'text-on-crossing is disabled'}
    peer_id = current.get('peer_id')
    if not valid_id(peer_id):
        raise Failure('saved clipboard opt-in has an invalid peer identity; '
                      'fix or remove clipboard.json')
    if adjacent_peer(peer_id) is None:
        raise Failure('saved clipboard opt-in names a peer that is not adjacent '
                      'in the saved layout; refusing to guess')
    me = g.identity()
    layout = g.read_json(g.CONFIG / 'layout.json', None)
    if not isinstance(layout, dict):
        raise Failure('no saved Glide layout')
    content = g.render_config(layout, me)
    try:
        existing = g.LAN_CONFIG.read_text()
    except OSError:
        existing = None
    if existing == content:
        return {'changed': False, 'reason': 'engine config already matches the saved opt-in'}
    g.atomic(g.LAN_CONFIG, content)
    return {'changed': True, 'reason': 'engine config updated to match the saved opt-in'}


PAIR_COMMANDS = {'pair-enable': pair_enable, 'pair-disable': pair_disable,
                 'pair-status': pair_status}


def json_cli(fn, *args):
    """JSON stdout protocol: {"ok": true, "result": ...} / {"ok": false, "error": ...}."""
    try:
        print(json.dumps({'ok': True, 'result': fn(*args)}))
        return 0
    except (Failure, ValueError, OSError, TimeoutExpired) as exc:
        # Never include clipboard contents in any error path.
        print(json.dumps({'ok': False, 'error': str(exc)}))
        return 1


def remote_cli(action):
    try:
        raw = sys.stdin.read(PAIR_PAYLOAD_MAX + 1)
        if len(raw) > PAIR_PAYLOAD_MAX:
            raise Failure('remote payload is too large')
        payload = json.loads(raw) if raw.strip() else None
        result = remote_dispatch(action, payload)
        print(json.dumps({'ok': True, 'result': result}))
        return 0
    except (Failure, ValueError, OSError) as exc:
        print(json.dumps({'ok': False, 'error': str(exc)}))
        return 1


def main(argv):
    try:
        if len(argv) == 2 and argv[0] in PAIR_COMMANDS:
            return json_cli(PAIR_COMMANDS[argv[0]], argv[1])
        if argv == ['reconcile']:
            return json_cli(reconcile)
        if len(argv) == 2 and argv[0] == '_remote':
            return remote_cli(argv[1])
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
              'enable <peerid> | disable | status | reconcile | '
              'pair-enable <peerid> | pair-disable <peerid> | pair-status <peerid>',
              file=sys.stderr)
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

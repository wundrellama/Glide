<p align="center">
  <img src="assets/glide-hero.png" alt="Glide — One desk. Every machine. An electric lime pointer flows across three dark displays." width="100%">
</p>

<p align="center"><strong>Mouse and keyboard sharing for OMARCHY.</strong><br>Move to the edge. Keep working.</p>

<p align="center">
  <img alt="Version" src="https://img.shields.io/badge/version-0.2.4-c5ff36?style=flat-square&labelColor=111111">
  <img alt="Platform" src="https://img.shields.io/badge/platform-Omarchy%20%2F%20Hyprland-c5ff36?style=flat-square&labelColor=111111">
  <img alt="Network" src="https://img.shields.io/badge/network-LAN%20only-c5ff36?style=flat-square&labelColor=111111">
</p>

Glide is an Omarchy plugin. It lets your pointer cross from one Omarchy computer to the next, with your keyboard following along. Arrange your machines in the Omarchy panel, pair them, and work across your desk with one set of controls. This is input sharing, not screen streaming or file transfer; clipboard contents are not shared unless you explicitly enable the optional text-on-crossing feature described below.

> **Fork notice:** this repository (`wundrellama/Glide`) is a fork maintained by **wundrellama**. The original project is by Fred Nix — see [Credits and licensing](#credits-and-licensing).

## Your whole desk, connected

| Feature | What it does |
| --- | --- |
| **Cross an edge** | Move between neighboring computers with your mouse. The keyboard follows the active handoff. |
| **Map your desk** | Drag up to nine computers onto a 5 × 3 grid. Arrange neighbors left, right, above, or below. Arrow keys also move the selected machine. |
| **Discover nearby sessions** | Find active Glide desktops on your physical private LAN. Discovery never grants permission to control a machine. |
| **Pair through SSH** | Verify each device's identity through your SSH login before applying a layout. |
| **Apply one layout** | Distribute the arrangement to participating machines, with rollback attempted if deployment fails. |
| **Pause from the bar** | Check live status and pause or resume sharing. Right-click the bar icon for a quick toggle; the action is synchronized to the paired machine over the verified SSH link. |
| **Encrypted connections** | The bundled engine uses DTLS with paired certificate fingerprints in both directions. |
| **Optional text-on-crossing** | Disabled by default. When enabled per adjacent pair from the panel, the machine you cross away from pushes its current clipboard text (only) to the machine you cross onto. See below. |
| **Emergency return** | Press **Left Ctrl + Left Shift + Esc** on the sending keyboard to release capture. |

<p align="center"><img src="assets/glide-icon.png" alt="Glide's black metal G and lime cursor emblem" width="180"></p>

## Install Glide once

Install Glide on the Omarchy desktop you are using now. After SSH verifies another unlocked Omarchy desktop, Glide transfers its built bundle and installs the plugin and services there automatically. Glide uses Avahi for discovery and UDP port 4242 for input sharing.

```bash
git clone https://github.com/wundrellama/Glide.git
cd Glide
./install.sh
```

The installer builds the bundled engine, installs the bar plugin and user services, starts Avahi, and configures LAN-scoped UFW rules. It may ask for `sudo` for packages, device access, Avahi, and firewall setup. With another firewall, configure the LAN rules yourself; the installer currently expects UFW.

**Upgrading from the original plugin:** the installer automatically disables the old plugin and retains it under `~/.local/state/omarchy/glide/plugin-backups/`, outside the Omarchy plugin directory. Pairing and clipboard settings remain unchanged.

Glide uses Lan Mouse's single capture/emulation ownership state machine. It does not run a second evdev ownership watcher, avoiding feedback from injected remote pointer events.

An unrelated existing Lan Mouse configuration is not overwritten. Back it up and move it before installing. Do not run another Lan Mouse daemon alongside Glide: both use the same IPC socket and configuration directory.

## SSH requirements

You install Glide once on the Omarchy desktop you are using. For each additional Omarchy desktop, Glide needs:

- SSH server access on the LAN (`ssh user@machine` must work).
- The remote host-key prompt accepted once, and a password or SSH key that can log in as that user.
- An active Omarchy user session; it does not need to be the focused desktop while pairing.
- Direct private-LAN IPv4 reachability. VPN, public-IP, and IPv6-only paths are not supported.

Glide verifies the SSH host identity interactively, transfers the already-built Glide bundle, asks for the remote user’s sudo password in the SSH terminal, installs the remote dependencies and user services, and starts them. If that account cannot use sudo, Glide stops with a clear error and the remote machine must be installed by an Omarchy administrator first. You do not separately install Glide on every machine. SSH handles installation, layout updates, and paired controls. Optional clipboard transfers also use SSH; mouse and keyboard traffic uses the paired DTLS connection on UDP 4242. The optional text-on-crossing feature has stricter SSH requirements (non-interactive public-key authentication in both directions); see its section below.

## Set up your desk

1. Open the mouse icon in the Omarchy bar.
2. Select a nearby Omarchy machine and choose **Install & authorize**. The terminal asks for the normal SSH host-key confirmation and login authentication. Verify the host identity before accepting it; Glide installs itself there over that authenticated SSH connection.
3. Choose **Add selected**, then drag the machine to match its physical position.
4. Choose **Apply layout**. Repeat for any additional computers.
5. Cross the corresponding screen edge to start sharing.

Connections follow the nearest machine in the same row or column. Diagonal-only arrangements do not create a connection. On the receiving computer, its own mouse or keyboard reclaims local control.

If you release control while resting on a screen edge, move inward before crossing again. Glide suppresses immediate recapture for 250 ms and rejects entry retries for 500 ms after physical takeover.

**Coordinate-aware crossing:** the engine carries the crossing point along an edge (a normalized fraction) through the handoff in both directions. When a peer's pointer enters this machine, the pointer is placed at the entered edge at that fraction before subsequent input arrives; when control returns to this machine, the capture side warps the pointer just inside the same relative edge position via the compositor's `wp_pointer_warp_v1` protocol, instead of resurrecting it wherever it was parked while hidden. Emergency release, disconnects, and physical takeover never warp. Both machines must run the rebuilt engine together — the wire format changed with no compatibility fallback for older builds. Edge coordinate crossing has been verified in prior work with both peers on the same build; if a compositor does not advertise pointer warping, the engine logs a warning at startup and returns behave as before.

## Troubleshooting

**Release a trapped pointer:** press Left Ctrl + Left Shift + Esc on the sending keyboard. To pause sharing from a terminal on either computer:

```bash
systemctl --user stop omarchy-glide.service
```

Resume with `systemctl --user start omarchy-glide.service`. The bar action is the convenient synchronized control: pause on either paired machine and resume on either one.

**Inspect the services:**

```bash
systemctl --user status omarchy-glide omarchy-glide-discovery
journalctl --user -u omarchy-glide -n 80
python3 scripts/glide-control.py status
```

**No nearby machines:** log into an unlocked Omarchy desktop and check Avahi plus LAN mDNS (UDP 5353). VPN, container, loopback, public-IP, and IPv6-only networks are not discovered.

**A pointer is trapped:** press Left Ctrl + Left Shift + Esc to release capture, then move inward before crossing again.

**Network address changed:** rescan and reapply the layout. Saved peer addresses and firewall rules are not a general roaming-network manager.

```bash
./uninstall.sh
```

Uninstall disables the plugin and services and removes recorded Glide firewall rules. It retains pairing identities, source, device-access rules, and recovery files.

## Optional text-on-crossing (disabled by default)

Glide can optionally push your clipboard **text** to the machine you cross onto. It is **disabled by default** and shares nothing until you explicitly turn it on for a specific adjacent pair. When enabled and your pointer crosses onto the paired peer, the machine you are leaving pushes its current UTF-8 text selection to that peer over the existing authorized SSH channel, and the peer installs it with `wl-copy`.

**Enable from the panel:** open the Glide panel, select a saved adjacent peer in the layout, and flip the text-sharing toggle. The toggle runs a paired enable (or disable) that validates both machines, writes the opt-in on each side, regenerates each engine configuration, and restarts each active Glide service automatically (paused services stay paused) — no manual **Apply layout** is needed for the toggle. The service's `ExecStartPre` reconcile step re-derives the engine configuration from the saved opt-in on every start, so the setting persists across service and session restarts. On failure, Glide attempts to restore both machines to their prior state and reports any recovery failure.

**Direction and limits:**

- Each hook pushes in one direction: the machine you cross *away from* sends; the machine you cross *onto* receives. Both ends opt in towards each other, so crossings in either direction carry text. This is not continuous sync, and returning a remotely controlled pointer or reclaiming local control does not guarantee a reverse clipboard transfer.
- Text only: `text/plain;charset=utf-8` (or `text/plain`), at most 1 MiB, exact bytes preserved. No continuous synchronization, no files, no images. Anything flagged secret (`CLIPBOARD_STATE=sensitive`, password-manager MIME hints) is skipped silently — these exclusions are best-effort.
- Both ends must opt in and name each other, both sessions must be active and unlocked, and the Glide service must be running on both. Anything else fails closed and shares nothing.

**Prerequisites (no new keys are provisioned):** the interactive password SSH login used during installation is **not sufficient**. Non-interactive SSH public-key authentication must already work in **both directions** between the paired machines — transfers use the existing strict host-identity, `BatchMode` SSH setup and never prompt. Password-only logins cannot transfer text. Agent-backed keys work only while the key remains available. `wl-copy`/`wl-paste` (wl-clipboard) must be installed on both machines.

**CLI fallback:** the same paired operations are available from one machine with the peer's 32-hex device identity (available in `network.py inventory` output):

```bash
read -r -p "Peer device ID: " PEER_ID
python3 ~/.config/omarchy/plugins/wundrellama.glide/scripts/glide_clipboard.py pair-enable "$PEER_ID"
python3 ~/.config/omarchy/plugins/wundrellama.glide/scripts/glide_clipboard.py pair-status "$PEER_ID"
python3 ~/.config/omarchy/plugins/wundrellama.glide/scripts/glide_clipboard.py pair-disable "$PEER_ID"
```

Run from either machine. Enable and disable configure both ends. `pair-status` is read-only and reports the combined local and remote state.

**Verification status:** text crossing has been exercised in both directions on a developer pair of machines. That is not a blanket guarantee across all platforms, compositors, or clipboard sources.

**Risks:** secret detection is best-effort: `CLIPBOARD_STATE` is normally provided to clipboard watchers, not this entry hook, and not every password manager offers a recognizable MIME hint. Unmarked passwords can therefore be transferred. Whatever plain text sits in the sender's clipboard at the moment of a crossing is sent to the peer's clipboard — including text you forgot was there, and over a crossing you made by accident. The SSH channel is authenticated and encrypted, but the text lands in the receiver's clipboard unprotected and may persist in its clipboard history. Clipboard contents are never logged.

## Credits and licensing

Originally built by **Fred Nix** for Omarchy, powered by **[Lan Mouse](https://github.com/feschber/lan-mouse)** by Ferdinand Schober and contributors. Vendored baseline: `0d2190e787db9e482e17d4c16fb7c00ef4841bcb` (Lan Mouse 0.11.0 source). This fork is maintained by **wundrellama**; all original author attribution is retained.

The Glide plugin and helpers use the [MIT license](LICENSE). The bundled Lan Mouse engine and its modifications use [GPL-3.0-or-later](engine/LICENSE). Both licenses and upstream attribution are retained.

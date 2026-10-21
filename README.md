# vr-test-mcp

An MCP server that gives an agent a virtual SteamVR headset. It drives stock ALVR 21: the server joins ALVR's streamer as a headset client over loopback, so SteamVR sees a headset with two Valve Index controllers and optional hand tracking. The agent can start and stop SteamVR, launch a VR game, move the head and hands, press buttons, take screenshots of what the headset streams, and read the game's haptics and logs. It does not patch ALVR and uses no OpenXR layer. It needs a logged-in Linux desktop session.

## Requirements

- Linux desktop session with native Steam. Flatpak Steam is refused.
- SteamVR, installed and run once.
- The ALVR streamer at the pinned version, 21.0.0-dev14 (nightly). Any 21.x release works once a stable one ships. Nightlies must match exactly. ALVR publishes nightlies as releases of the `alvr-org/ALVR-nightly` repository on GitHub. Take one whose version is 21.0.0-dev14 (the crates are pinned to commit `ebec107b`), download `alvr_streamer_linux` and unpack it anywhere.
- An AMD or Intel GPU for SteamVR. ALVR 21 direct mode works on AMD. On NVIDIA it is broken upstream (ALVR #3421), so an NVIDIA-only machine is refused.
- VA-API H.264 encoding on that GPU, and a default audio input (ALVR's client opens it at every connection).

`vr_status` lists every startup check with its reason and the fix.

## Build

You need Rust 1.98 or newer. Install your distro's ALSA and PipeWire development packages and clang, for example `alsa-lib-devel`, `pipewire-devel` and `clang` on Fedora, or `libasound2-dev`, `libpipewire-0.3-dev` and `clang` on Debian and Ubuntu. Then:

```
cargo build --release
```

The binary is `target/release/vr-test-mcp`. Plain `cargo test` needs no Steam.

## Config

Everything is detected. An optional `$XDG_CONFIG_HOME/vr-test-mcp/config.toml` overrides it. Unknown keys are rejected.

The server reads the file at startup, at each `vr_start`, and at `vr_status` when the file changed or a core check fails. It applies a new version only while nothing runs, so a fix to a failing check shows up at the next `vr_status` without restarting the MCP. If SteamVR or a session is up, run `vr_stop` first so your edit takes effect. With a malformed file the server keeps running: the `config` check fails, and `vr_start` stays hidden until the file parses.

| Key | Meaning |
| --- | --- |
| `steam_dir` | Steam root. Default: `~/.steam/root`, then `~/.local/share/Steam`. |
| `steamvr_dir` | SteamVR install. Default: found through ALVR, then under `steam_dir`. |
| `streamer_dir` | The unpacked `alvr_streamer_linux`. Default: the driver already registered with SteamVR. |
| `steam.env` | Environment variables for Steam, table of strings. |
| `steam.args` | Extra arguments for Steam, list of strings. |
| `alvr_settings` | ALVR session path to value, applied after the built-in defaults on every `vr_start`. |
| `timeouts.streaming_s` | Wait for the headset to stream. Default 240. |
| `timeouts.app_s` | Wait for a launched app's scene. Default 180. |

`steam.env` and `steam.args` apply only when the MCP starts Steam. If Steam is already running, the MCP uses it as it is and cannot change its environment. Steam gets only the desktop session's variables (`HOME`, `PATH`, `DISPLAY`, `WAYLAND_DISPLAY`, `XDG_*`, locale, D-Bus and Pulse) plus `steam.env`. The rest of the agent's environment, API keys included, stays out of Steam and its games.

To find `<vendor:device>`, look for the `[vvvv:dddd]` pair on the GPU's line in `lspci -nn`, or the vendorID and deviceID in `vulkaninfo --summary`.

```toml
streamer_dir = "/path/to/alvr_streamer_linux"

[steam]
env = { MESA_VK_DEVICE_SELECT = "<vendor:device>!" }
```

Set `STEAM_FORCE_CLIENT` in `steam.env` to the directory name of the older 32-bit Steam client under the Steam root when the new Steam client lacks SteamVR's launcher service.

### Hybrid GPUs

SteamVR's compositor runs on the GPU Steam uses, and a Proton or OpenXR game has to render on the same GPU. A game whose DXVK picks the other GPU crashes inside `vrclient`. Select the SteamVR GPU for Steam through `steam.env` as above, and give the game's launch options the Vulkan ICD of that GPU plus the same device selection:

```
VK_DRIVER_FILES=<icd json of the SteamVR GPU> MESA_VK_DEVICE_SELECT=<vendor:device>! %command%
```

The MCP never edits your desktop launchers or a game's launch options.

## Registration

One user-scope entry serves both Claude Code and omp, because both read `mcpServers` from `~/.claude.json`:

```
claude mcp add --scope user vr-test-mcp -- <path>/target/release/vr-test-mcp
```

Do not add the same server to `~/.omp/agent/mcp.json` as well. The server's startup checks only read state, so registering it starts nothing. Logs go to stderr and stdout carries the protocol only.

Only `vr_status` and `get_logs` are always listed. Every other tool appears while its prerequisites hold, and the server sends `notifications/tools/list_changed` when that changes. A call to a gated tool answers `not ready: <reason>`.

### Host notes

- Claude Code acts on `tools/list_changed`. For protocol 2026-07-28 it needed `subscriptions/listen`, which the server serves.
- omp handles the notification at the protocol level (checked with a manual stdio probe). It is not verified live, because omp's API credit ran out during testing.

## Tools

| Tool | What it does |
| --- | --- |
| `vr_status` | Startup checks and the state of Steam, SteamVR, ALVR and the headset. |
| `vr_start` | Start Steam if needed, SteamVR with ALVR and the headset, and wait for streaming. |
| `vr_stop` | Shut SteamVR down and disconnect the headset. |
| `vr_restore` | Put back the SteamVR and ALVR setup from before the first `vr_start`. |
| `screenshot` | PNG of what the headset streams, both eyes or one. |
| `get_tracked_state` | What SteamVR tracks: devices, poses, velocities, skeletons and curls, the scene app. |
| `get_logs` | ALVR events plus the tails of SteamVR's vrserver, vrcompositor and vrmonitor logs. |
| `get_haptics` | Haptic events the app sent. |
| `launch_app` | Launch a Steam app or shortcut and wait for its scene. |
| `stop_app` | Stop the scene app. |
| `set_head_pose` | Set the head pose. |
| `move_head` | Move the head relative to its pose. |
| `animate_head` | Move the head through waypoints over time. |
| `set_controller_pose` | Set a controller's pose. |
| `move_controller` | Move a controller relative to its pose. |
| `animate_controller` | Move a controller through waypoints over time. |
| `set_controller_state` | Connect or disconnect a controller. |
| `set_finger_curl` | Curl and splay each finger. |
| `set_hand_mode` | Per hand, hold a controller or be a bare tracked hand. |
| `set_hand_skeleton` | Send raw 26-joint hand data. |
| `set_hand_gesture` | Pose a hand with a named gesture or custom curls. |
| `press_button` | Press and hold an Index button. |
| `release_button` | Release a button. |
| `touch_button` | Touch or untouch a capacitive sensor. |
| `set_trigger` | Set the trigger value. |
| `set_grip` | Set the grip value and force. |
| `set_thumbstick` | Set the thumbstick position, click and touch. |
| `set_trackpad` | Set the trackpad position, force and touch. |
| `set_playspace` | Set the play area size. |
| `recenter` | Send the play area to ALVR again. |
| `run_sequence` | Run timed steps of the rig tools, checked before any runs. |
| `set_alvr_settings` | Set ALVR session values by path, restarting SteamVR when ALVR needs it. |
| `set_hand_tracking_flavour` | Bare hands as the Index controller, or as separate skeletal hand trackers. |

Coordinates are SteamVR standing space (+Y up, -Z forward). Tool arguments use meters, degrees and seconds. The default session sets ALVR's `recentering_mode` to `Stage`, so poses you send are absolute.

## Behaviors worth knowing

- SteamVR gives background OpenVR readers no input until the scene app (SteamVR Home) is running. `get_tracked_state` skeletons stay inactive before that.
- SteamVR reserves the Index system click for its dashboard. System touch reaches apps.
- Steam can block a launch on a desktop dialog (`ShowInterstitials` or `ShowLaunchOption`, for example "VR Theater Screen" for an app Steam treats as non-VR). `launch_app` and `vr_start` fail fast and name the dialog. Dismiss it on the desktop.
- A second Steam client running at the same time (Flatpak Steam, for example) can take over Steam's runtime launch service. The Steam that the MCP talks to then stops accepting `steam://` URLs. `vr_start` and `launch_app` fail within 20 s with "Steam did not take ...". Close the other Steam, or start this Steam before it, then restart this Steam.
- `set_alvr_settings` changes last until the next `vr_start`, which applies the defaults and `config.toml` `alvr_settings` again.
- `vr_start`, `vr_stop`, `set_alvr_settings`, `set_hand_tracking_flavour`, `launch_app` and `run_sequence` return only when they are done, which can take minutes.
- `vr_stop` stops only a SteamVR that runs ALVR. If you started SteamVR without ALVR, close it yourself.

### Restarts are slow

ALVR's TCP stream socket spins at end of stream on loopback (`sockets/src/stream_socket/tcp.rs:137-147`), and the server's receive thread never finishes after the headset drops. As a result:

- A headset reconnect needs a SteamVR restart.
- `vr_stop` usually ends SteamVR with signals, which takes about 30 s.
- A restart-relevant settings change takes about 80 s.
- If the MCP exits while the headset streams, vrserver keeps one CPU core busy until SteamVR stops.

This is an upstream ALVR issue that is worth reporting.

## Restore

The first `vr_start` writes `$XDG_STATE_HOME/vr-test-mcp/restore.json`. Later starts never overwrite it. It holds:

- the `external_drivers` list from `openvrpaths.vrpath`;
- the whole `steamvr.vrsettings`;
- the ALVR session file, and any file ALVR created next to it;
- the `vrcompositor` wrapper symlink, if SteamVR had one.

`vr_start` registers the pinned ALVR driver in place of any other ALVR driver, sets `forcedDriver` to `alvr_server`, clears `blocked_by_safe_mode` for the ALVR driver, and writes the MCP's ALVR settings with a trusted `127.0.0.1` client.

A signal-based SteamVR stop makes SteamVR set `blocked_by_safe_mode` on other drivers and rewrite some settings. `vr_stop` puts the driver blocks back, and `vr_restore` writes back the whole `steamvr.vrsettings` snapshot, so anything you changed in SteamVR between start and restore is lost. `vr_restore` deletes `restore.json` afterwards and refuses while SteamVR runs. A crashed MCP can still restore, since the state is a file.

## Known ALVR gaps

Stock ALVR 21 is not patched here. Gaps are reported, not worked around.

- The left hand tracker registers its buttons from the right-hand map (`Controller.cpp:112-117`).
- The loopback TCP spin described above.

Not covered: Monado, WiVRn, an OpenXR API layer, Windows, Flatpak Steam, headless or CI use, Steam Frame controllers, audio, eye, face and body tracking, more than one virtual headset.

## Bumping the ALVR pin

Change `rev` for every `alvr_*` dependency in `Cargo.toml`, install the matching streamer, and run `cargo test`. Then run the live tests, which need a desktop with Steam and a VR game: `cargo test --test live -- --ignored --test-threads=1`, with `VR_TEST_GAME_ID` set to the game's Steam app id or shortcut game id. The `xr_probe` example (`cargo build --release --example xr_probe`) is a native OpenXR sample that the live tests use to check inputs and haptics.

The live tests leave SteamVR running for the next one. Only `live_zz_stop_and_restore` stops it and restores the setup. After running a single live test, run that one too, or call `vr_stop` and `vr_restore`.

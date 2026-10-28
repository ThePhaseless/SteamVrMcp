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
| `app_logs` | Table of Steam app id to glob patterns of the game's own log files, read by `read_app_log` and `wait_for_log`. |

`steam.env` and `steam.args` apply only when the MCP starts Steam. If Steam is already running, the MCP uses it as it is and cannot change its environment. When Steam runs without the `steam.env` values, `vr_start` fails at once and asks you to close Steam first. Steam gets only the desktop session's variables (`HOME`, `PATH`, `DISPLAY`, `WAYLAND_DISPLAY`, `XDG_*`, locale, D-Bus and Pulse) plus `steam.env`. The rest of the agent's environment, API keys included, stays out of Steam and its games.

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

### Game logs

`app_logs` maps a Steam app id (decimal, as a string) to patterns. Each pattern may use `${compatdata}` (`<library>/steamapps/compatdata/<id>`), `${prefix}` (`${compatdata}/pfx`) and `${install}` (the install dir from the app manifest; empty for a shortcut, so its patterns match nothing). `**` is refused, so write one `*` per directory level. An unknown variable, a bad pattern or an id that is not a plain decimal fails the `config` check. This example reads a BepInEx log and a Unity player log of a Proton game:

```toml
[app_logs]
"<app_id>" = [
  "${install}/BepInEx/LogOutput.log",
  "${prefix}/drive_c/users/steamuser/AppData/LocalLow/*/*/Player.log",
]
```

Edits to `[app_logs]` apply after `vr_stop`, like the rest of the config.

## Registration

One user-scope entry serves both Claude Code and omp, because both read `mcpServers` from `~/.claude.json`:

```
claude mcp add --scope user vr-test-mcp -- <path>/target/release/vr-test-mcp
```

Do not add the same server to `~/.omp/agent/mcp.json` as well. The server's startup checks only read state, so registering it starts nothing. Logs go to stderr and stdout carries the protocol only.

For most hosts, only `vr_status`, `get_logs`, `vr_acquire` and `vr_release` are always listed. Every other tool appears while its prerequisites hold, and the server sends `notifications/tools/list_changed` when that changes. Hosts in `HOSTS_WITHOUT_LIST_CHANGED` (`src/server.rs`) get every tool listed. In every case, a call to a gated tool answers `not ready: <reason>`.

### Host notes

- Claude Code gets every tool listed. It acts on `tools/list_changed` (for protocol 2026-07-28 through `subscriptions/listen`, which the server serves), but its subagents keep the tool list they were spawned with, and a refresh mid-turn removes tools the model already planned to call.
- omp handles the notification at the protocol level (checked with a manual stdio probe). It is not verified live, because omp's API credit ran out during testing.

## Tools

| Tool | What it does |
| --- | --- |
| `vr_status` | Startup checks and the state of Steam, SteamVR, ALVR and the headset. |
| `vr_acquire` | Take or renew the SteamVR lease under a name you choose. |
| `vr_release` | Give the lease up so another agent can take it. |
| `vr_start` | Start Steam if needed, SteamVR with ALVR and the headset, and wait for streaming. |
| `vr_stop` | Shut SteamVR down and disconnect the headset. |
| `vr_restore` | Put back the SteamVR and ALVR setup from before the first `vr_start`. |
| `screenshot` | PNG from one of three sources: `headset` (what the headset streams, default), `compositor` (SteamVR's uncompressed scene textures) or `desktop` (the whole desktop through xdg-desktop-portal). Both eyes or one; `desktop` only gives both. |
| `get_tracked_state` | What SteamVR tracks: devices, poses, velocities, skeletons and curls, the scene app. |
| `get_logs` | ALVR events plus the tails of SteamVR's vrserver, vrcompositor and vrmonitor logs. |
| `read_app_log` | Lines of a game's own log files, as `app_logs` says. Read on with `since_seq`. |
| `wait_for_log` | Wait for a game log line that matches a regex, or a `fail_pattern`, or the timeout. |
| `get_haptics` | Haptic events the app sent. |
| `launch_app` | Launch a Steam app or shortcut and wait for its scene. `via` is `steam` (default) or `proton`, which runs the game's compatibility tool directly with `exe`, `args` and `env`. |
| `stop_app` | Stop the scene app. For a `via proton` launch it stops the whole Proton tree. |
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
| `close_dashboard` | Click the right controller's system button once if SteamVR's dashboard is open, which keeps a game unfocused. `launch_app` does it by itself. |
| `press_button` | Press and hold an Index button. |
| `release_button` | Release a button. |
| `touch_button` | Touch or untouch a capacitive sensor. |
| `set_trigger` | Set the trigger value. |
| `set_grip` | Set the grip value and force. |
| `set_thumbstick` | Set the thumbstick position, click and touch. |
| `set_trackpad` | Set the trackpad position, force and touch. |
| `set_proximity` | Tell SteamVR the headset is worn or taken off. |
| `set_playspace` | Set the play area size. |
| `recenter` | Send the play area to ALVR again. |
| `get_frame_timing` | Wait a window, then report SteamVR compositor frame timings and ALVR's latency statistics over it. |
| `record_audio` | Record the default audio output to a WAV file. |
| `audio_level` | RMS and peak level of the default audio output over a short window. |
| `run_sequence` | Run timed steps of the rig tools, checked before any runs. |
| `set_alvr_settings` | Set ALVR session values by path, restarting SteamVR when ALVR needs it. |
| `set_hand_tracking_flavour` | Bare hands as the Index controller, or as separate skeletal hand trackers. |

`launch_app via proton` takes `exe` relative to the install dir (it must stay inside it) and refuses `STEAM_COMPAT_*`, `SteamAppId` and `SteamGameId` in `env`. The game's Steam launch options do not apply, and Steam shows no dialogs. Output of the launcher goes to the `run_log` that `launch_app` returns.

Coordinates are SteamVR standing space (+Y up, -Z forward). Tool arguments use meters, degrees and seconds. The default session sets ALVR's `recentering_mode` to `Stage`, so poses you send are absolute.

## Behaviors worth knowing

- Two agents on one machine share one SteamVR through a lease. Any tool that changes something takes the lease by itself, for 30 minutes unless `vr_acquire` sets `ttl_s`, and renews it once half is left. While another live MCP holds it, the mutating tools disappear from the list and answer `not ready: SteamVR is leased to ...`. Read-only tools (`vr_status`, `get_logs`, `get_haptics`, `get_tracked_state`, `screenshot`, `read_app_log`, `wait_for_log`, `get_frame_timing`, `record_audio`, `audio_level`) stay open. An observer never streams, so for it `get_frame_timing` is hidden and `screenshot` takes only the compositor and desktop sources. The holder's MCP keeps renewing the lease while it manages a session, so the lease ends at `vr_release`, a `vr_stop` that leaves no SteamVR process behind, a successful `vr_restore`, or the holder's exit. A lease whose MCP has died is free at once, and one whose MCP no longer manages a session can be taken over once it expires. `vr_status` shows the owner and whether it is this MCP.
- Times come in two forms. `t_s` is seconds since the MCP started and `at` is the same instant in UTC (RFC 3339). `vr_status` reports `now_t_s`, `now_at` and `t0_at`, so lines from different tools can be put in order.
- A signal-based SteamVR stop leaves `blocked_by_safe_mode` blocks on other drivers. The MCP clears SteamVR's safe-mode add-on blocks before each cold start, because 2.18.4 otherwise stops the launch on a desktop dialog; `vr_status` lists the ones present as `blocked_drivers`, and `vr_restore` puts the original file back.
- `launch_app via proton` starts the game without Steam launch options and without Steam dialogs. Like the other `launch_app` mode, it needs a streaming headset first.
- The waits end with an error that names what ended them: `failed: <signal>` for a known failure, such as vrserver exiting, or `deadline N s`. `wait_for_log` answers `result: "match"`, `"fail"` or `"timeout"`, and fails with `failed: app exited` once the Proton launcher or the scene app it saw at its start is gone. `timeouts.streaming_s` (240 s) covers the whole `vr_start`, restarts included.
- The first `screenshot {source: "desktop"}` can make the desktop show xdg-desktop-portal's permission prompt. Approve it once. If nobody answers, the call times out and cancels the request.
- `record_audio` and `audio_level` hear the whole default output, other apps included, not one game. WAV files go to the MCP's private runtime dir, under `audio/`, and stay until logout.
- SteamVR gives background OpenVR readers no input until the scene app (SteamVR Home) is running. `get_tracked_state` skeletons stay inactive before that.
- SteamVR opens its dashboard about 10 s after no scene app runs (`vr_start` reports `dashboard_visible`), so call `close_dashboard` once your game reaches its scene.
- After a SteamVR or Steam restart a launched game can come up with the SteamVR dashboard open, and its OpenXR session stays VISIBLE, not FOCUSED. `launch_app` clicks the right controller's system button once when `vr_status` shows `dashboard_visible: true` (and notes it in `notes`); for a game started another way, call `close_dashboard`.
- SteamVR reserves the Index system click for its dashboard. System touch reaches apps.
- Steam can block a launch on a desktop dialog (`ShowInterstitials` or `ShowLaunchOption`, for example "VR Theater Screen" for an app Steam treats as non-VR). `launch_app` and `vr_start` fail fast and name the dialog. Dismiss it on the desktop.
- A second Steam client running at the same time (Flatpak Steam, for example) can take over Steam's runtime launch service. The Steam that the MCP talks to then stops accepting `steam://` URLs. `vr_start` and `launch_app` fail within 20 s with "Steam did not take ...". Close the other Steam, or start this Steam before it, then restart this Steam.
- Two Steam clients under one Linux user (native plus Flatpak, for example) cannot both run a game. The native game's steamclient dies about 28 s in with "fatal stalled cross-thread pipe", even with the Flatpak sandboxed. Run one Steam.
- `set_alvr_settings` changes last until the next `vr_start`, which applies the defaults and `config.toml` `alvr_settings` again.
- `vr_start`, `vr_stop`, `set_alvr_settings`, `set_hand_tracking_flavour`, `launch_app`, `run_sequence` and `wait_for_log` return only when they are done, which can take minutes.
- `vr_stop` first ends a game that `launch_app via proton` started and lists its pids in `stopped_app`, then stops SteamVR.
- `vr_stop` stops only a SteamVR that runs ALVR. If you started SteamVR without ALVR, close it yourself.

### Restarts are slow

ALVR's TCP stream socket spins at end of stream on loopback (`sockets/src/stream_socket/tcp.rs:137-147`), and the server's receive thread never finishes after the headset drops. As a result:

- A headset reconnect needs a SteamVR restart.
- `vr_stop` usually ends SteamVR with signals, which takes about 30 s.
- A restart-relevant settings change takes about 80 s.
- If the MCP exits while the headset streams, vrserver keeps one CPU core busy until SteamVR stops.
- The virtual headset lives inside the MCP process. When the MCP or the agent session that runs it restarts, a running OpenXR app loses focus (FOCUSED back to SYNCHRONIZED). The next `vr_start` adopts SteamVR and starts a new headset.

This is an upstream ALVR issue that is worth reporting.

## Restore

The first `vr_start` writes `$XDG_STATE_HOME/vr-test-mcp/restore.json`. Later starts never overwrite it. It holds:

- the `external_drivers` list from `openvrpaths.vrpath`;
- the whole `steamvr.vrsettings`;
- the ALVR session file, and any file ALVR created next to it;
- the `vrcompositor` wrapper symlink, if SteamVR had one.

`vr_start` registers the pinned ALVR driver in place of any other ALVR driver, sets `forcedDriver` to `alvr_server`, clears `blocked_by_safe_mode` for the ALVR driver, and writes the MCP's ALVR settings with a trusted `127.0.0.1` client.

A signal-based SteamVR stop makes SteamVR set `blocked_by_safe_mode` on other drivers and rewrite some settings. `vr_stop` and the next cold `vr_start` clear those blocks, and `vr_restore` writes back the whole `steamvr.vrsettings` snapshot, so anything you changed in SteamVR between start and restore is lost. `vr_restore` deletes `restore.json` afterwards and refuses while SteamVR runs. A crashed MCP can still restore, since the state is a file.

## Known ALVR gaps

Stock ALVR 21 is not patched here. Gaps are reported, not worked around.

- The left hand tracker registers its buttons from the right-hand map (`Controller.cpp:112-117`).
- The loopback TCP spin described above.

Not covered: Monado, WiVRn, an OpenXR API layer, Windows, Flatpak Steam, headless or CI use, Steam Frame controllers, per-app audio isolation, desktop window capture, eye, face and body tracking, more than one virtual headset.

## Bumping the ALVR pin

Change `rev` for every `alvr_*` dependency in `Cargo.toml`, install the matching streamer, and run `cargo test`. Then run the live tests, which need a desktop with Steam and a VR game: `cargo test --test live -- --ignored --test-threads=1`, with `VR_TEST_GAME_ID` set to the game's Steam app id or shortcut game id. The `xr_probe` example (`cargo build --release --example xr_probe`) is a native OpenXR sample that the live tests use to check inputs and haptics.

The live tests leave SteamVR running for the next one. Only `live_zz_stop_and_restore` stops it and restores the setup. After running a single live test, run that one too, or call `vr_stop` and `vr_restore`.

## Live verification checklist

Plain `cargo test` covers none of this. Run it through the MCP from a desktop session while no other agent uses SteamVR. Each line is the call and what passing looks like.

1. Clocks: `vr_status` shows `now_t_s`, `now_at` and `t0_at`. After `vr_start`, a `get_logs` vrserver line and an ALVR line from the same second have `t_s` within 1 s. A `get_haptics` event fired by `xr_probe` has `t_s` within 0.2 s of the probe's own log line.
2. Cold start: a cold `vr_start`, and a `set_alvr_settings` that restarts SteamVR, never fail with `failed: vrserver exited`.
3. Lease: start two MCP sessions. Session A calls `set_head_pose`. Session B's tool list drops the mutating tools within 2 s, `vr_status.state.owner.mine` is false in B, and B's `vr_start` answers `not ready: SteamVR is leased to mcp-<A pid>`. A runs `vr_release`, and B's mutating tools come back within 2 s. Kill A with SIGKILL while it holds the lease, and B can claim at once. While A runs a session and calls only read-only tools for over 30 minutes, A's lease stays renewed. While A restarts SteamVR, B's `get_tracked_state` fails or answers within 5 s and B keeps working afterwards.
4. Safe mode: `vr_start`, then kill vrmonitor with SIGKILL. Call `vr_status` until `blocked_drivers` lists the blocked drivers. `vr_stop`, and `blocked_drivers` is back to the snapshot's state. `vr_start` streams.
5. App logs: add `[app_logs]` for a Unity Proton game and `launch_app` it. `read_app_log` returns lines from the new `Player.log` only. `wait_for_log {pattern: "<a line the game prints at its menu>", fail_pattern: "Exception"}` answers `match` within the game's load time.
6. Proximity: the headset reports worn by default at each stream start (SteamVR 2.18 needs it to focus OpenXR apps). With an OpenXR app running, `set_proximity {on_head: false}` makes `get_tracked_state` show the HMD's `head_proximity: false`, and the app reports focus loss or idle. `on_head: true` reverses it. The events reach the Background reader. Also call `get_tracked_state` at once after `set_proximity`, both right after `vr_start` and after `launch_app`: it shows the new value.
7. Frame timing: with a game in VR, `get_frame_timing {window_s: 5}` returns a non-null `compositor` with `fps` near the stream fps, a non-null `alvr`, and empty `notes`. If `compositor` is null, note whether vrserver withholds timings from a Background client.
8. Compositor screenshot: `screenshot {source: "compositor", eye: "left"}` returns a PNG at the app's eye render size, sharper than `source: "headset"`. It works from a Background app. Check that vrserver can write `$XDG_STATE_HOME/vr-test-mcp/screenshots/` from its container, and whether the shot lands in the Steam screenshot library.
9. Proton launch: `launch_app {app_id, via: "proton", exe: "<game exe>", args: ["<arg>"], env: {"PROTON_LOG": "1"}}` reaches a running scene without a Steam dialog. The game sees the arg, and `run_log` has Proton's output. `stop_app` returns, and afterwards `/proc` has no process with that `STEAM_COMPAT_DATA_PATH` and app id. Launch again, then `vr_stop`: its `stopped_app` names the game, and a second proton launch of the same app reaches its scene. Run it once with Proton Experimental (the runtime entry point, where wine processes carry `SteamGameId` and `stop_app` sweeps them) and once with a custom tool.
10. Desktop screenshot: `screenshot {source: "desktop"}` returns the current desktop. Note whether a permission prompt appeared on the first call, and confirm the second call shows none.
11. Audio: play a test tone or the game's menu music. `audio_level {window_s: 1}` reads above -60 dBFS on both channels. Turn the head 90 degrees with `move_head {yaw: 90}`, and the L/R balance of a spatialized sound shifts. `record_audio {seconds: 3}` writes a WAV that plays back the same sound. Repeat on a mono and a 5.1 default sink.

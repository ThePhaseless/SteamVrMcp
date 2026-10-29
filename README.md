# vr-test-mcp

An MCP server that gives an AI agent a virtual SteamVR headset, so it can test VR apps on Linux with nobody wearing one.

The server joins a stock ALVR 21 streamer as a headset client over loopback. SteamVR then sees a headset with two Valve Index controllers and optional hand tracking. The agent can start and stop SteamVR, launch a VR game, move the head and hands, press buttons, and then check what happened: screenshots of what the headset receives, SteamVR's tracked poses and skeletons, haptic pulses, frame timing, audio levels and log lines. Input enters SteamVR at the driver level, so OpenXR and OpenVR games both work. ALVR is not patched and no OpenXR layer is installed.

## Requirements

- Linux with a logged-in desktop session (X11 or Wayland). Headless and CI machines are not supported.
- Native Steam. Flatpak Steam is refused.
- SteamVR, installed and started once.
- The ALVR streamer at the version the crates are pinned to: 21.0.0-dev14, a nightly. Nightlies must match exactly; once a stable 21.x ships, any 21.x release works. ALVR publishes nightlies as releases of `alvr-org/ALVR-nightly` on GitHub. Pick the one whose version is 21.0.0-dev14 (ALVR commit `ebec107b`), download `alvr_streamer_linux` and unpack it anywhere. Do not run ALVR's own dashboard while the MCP works: the MCP plays that role.
- An AMD or Intel GPU for SteamVR. ALVR 21 direct mode is broken on NVIDIA upstream (ALVR #3421), so a machine with only NVIDIA GPUs is refused. A hybrid laptop works if SteamVR runs on the AMD or Intel GPU (see [Hybrid GPUs](#hybrid-gpus)).
- VA-API H.264 encoding on that GPU (Mesa's `radeonsi` or Intel's media driver).
- A default audio input. ALVR's client opens it at every connection.
- Optional: PipeWire for `record_audio` and `audio_level`, and xdg-desktop-portal with your desktop's backend for desktop screenshots.

`vr_status` lists every startup check with its result and the fix.

## Build

You need Rust 1.98 or newer, clang (for bindgen), pkg-config, and the ALSA and PipeWire development packages: `alsa-lib-devel pipewire-devel clang` on Fedora, `libasound2-dev libpipewire-0.3-dev clang pkg-config` on Debian and Ubuntu. Then:

```sh
cargo build --release
```

The binary is `target/release/vr-test-mcp`. `cargo test` runs the unit and integration tests and needs no Steam.

If the build cannot find `alsa.pc` or `libpipewire-0.3.pc`, point `PKG_CONFIG_PATH` at the directory holding them. If bindgen cannot find `stddef.h`, pass your compiler's include directory through `BINDGEN_EXTRA_CLANG_ARGS`, for example `-I/usr/lib/gcc/x86_64-linux-gnu/14/include`.

## Register it with an MCP host

The server speaks MCP over stdio and takes no arguments. Registering it starts nothing: the startup checks only read state.

Claude Code:

```sh
claude mcp add --scope user vr-test-mcp -- /path/to/vr-test-mcp/target/release/vr-test-mcp
```

Other hosts take the same command in their usual `mcpServers` entry:

```json
{
  "mcpServers": {
    "vr-test-mcp": { "command": "/path/to/vr-test-mcp/target/release/vr-test-mcp" }
  }
}
```

Stdout carries only the protocol. Logs go to stderr (see [Logging](#logging)).

### Tool gating

Most hosts see only `vr_status`, `get_logs`, `vr_acquire` and `vr_release` at first. Every other tool appears while its prerequisites hold, and the server sends `notifications/tools/list_changed` when that changes.

Claude Code is the exception: it gets every tool listed from the start. It does act on `tools/list_changed` (for MCP protocol 2026-07-28 through `subscriptions/listen`, which the server serves), but its subagents keep the tool list they were spawned with, and a refresh in the middle of a turn removes tools the model already planned to call. Hosts like this are listed in `HOSTS_WITHOUT_LIST_CHANGED` in `src/server.rs`.

Whether listed or not, a tool whose prerequisites fail answers `not ready: <reason>`.

## Configuration

Paths are detected, so most machines need no config. An optional `$XDG_CONFIG_HOME/vr-test-mcp/config.toml` (usually `~/.config/vr-test-mcp/config.toml`) overrides the detection. Unknown keys are rejected.

| Key | Default | Meaning |
| --- | --- | --- |
| `steam_dir` | `~/.steam/root`, then `~/.local/share/Steam` | Steam root. |
| `steamvr_dir` | found through ALVR, then `<steam_dir>/steamapps/common/SteamVR` | SteamVR install. |
| `streamer_dir` | the ALVR driver already registered with SteamVR | The unpacked `alvr_streamer_linux`. |
| `steam.env` | empty | Environment variables for Steam, a table of strings. |
| `steam.args` | empty | Extra arguments for Steam, a list of strings. |
| `alvr_settings` | empty | ALVR session path to value, applied after the built-in defaults on every `vr_start`. |
| `timeouts.streaming_s` | 240 | How long `vr_start` waits for the headset to stream, restarts included. |
| `timeouts.app_s` | 180 | How long `launch_app` waits for the app's scene. |
| `app_logs` | empty | Steam app id to glob patterns of the game's own log files, for `read_app_log` and `wait_for_log`. |

A minimal file:

```toml
streamer_dir = "/path/to/alvr_streamer_linux"
```

The server reads the file at startup, at each `vr_start`, and at `vr_status` when the file changed or a core check fails. It applies a new version only while nothing runs, so after you fix a failing check, the next `vr_status` shows it without restarting the MCP. If SteamVR or a session is up, run `vr_stop` first. A malformed file does not stop the server: the `config` check fails, and `vr_start` stays unavailable until the file parses.

### Steam environment

`steam.env` and `steam.args` apply only when the MCP starts Steam. A Steam that is already running is used as it is, since its environment cannot change. If it runs without the `steam.env` values, `vr_start` fails at once and asks you to close Steam first.

Steam gets only the desktop session's variables (`HOME`, `PATH`, `DISPLAY`, `WAYLAND_DISPLAY`, `XDG_*`, locale, D-Bus and Pulse) plus `steam.env`. The rest of the agent's environment, API keys included, never reaches Steam or its games.

If the new Steam client lacks SteamVR's launcher service, set `STEAM_FORCE_CLIENT` in `steam.env` to the directory name of the older 32-bit Steam client under the Steam root.

### Hybrid GPUs

SteamVR's compositor runs on the GPU Steam uses, and a Proton or OpenXR game has to render on the same GPU. A game whose DXVK picks the other GPU crashes inside `vrclient`. Select the SteamVR GPU for Steam through `steam.env`:

```toml
[steam]
env = { MESA_VK_DEVICE_SELECT = "<vendor:device>!" }
```

Find `<vendor:device>` as the `[vvvv:dddd]` pair on the GPU's line in `lspci -nn`, or the vendorID and deviceID in `vulkaninfo --summary`. Then give the game's Steam launch options the Vulkan ICD of that GPU and the same device selection:

```
VK_DRIVER_FILES=<icd json of the SteamVR GPU> MESA_VK_DEVICE_SELECT=<vendor:device>! %command%
```

The MCP never edits your desktop launchers or a game's launch options.

### Game logs

`app_logs` maps a Steam app id (decimal, as a string) to glob patterns. A pattern may use:

- `${compatdata}`: `<library>/steamapps/compatdata/<id>`
- `${prefix}`: `${compatdata}/pfx`
- `${install}`: the install dir from the app manifest. It is empty for a non-Steam shortcut, so those patterns match nothing.

`**` is refused, because Proton prefixes link `z:` to `/`. Write one `*` per directory level. An unknown variable, a bad pattern or an id that is not a plain decimal fails the `config` check. This example reads a BepInEx log and a Unity player log of a Proton game:

```toml
[app_logs]
"<app_id>" = [
  "${install}/BepInEx/LogOutput.log",
  "${prefix}/drive_c/users/steamuser/AppData/LocalLow/*/*/Player.log",
]
```

### ALVR settings

`alvr_settings` takes ALVR's path syntax from the session JSON root:

```toml
[alvr_settings]
"session_settings.video.preferred_fps" = 90.0
```

The built-in defaults the MCP applies first: H.264 Baseline, foveated encoding off, view resolution scale 1.0, 72 fps, game audio and microphone streaming off, `Stage` recentering, tracked Index controllers with hand skeletons and gesture input, a manual button mapping that passes every Index input through, and client discovery off. `set_alvr_settings` changes last until the next `vr_start`, which applies the defaults and `alvr_settings` again.

## Tools

Coordinates are SteamVR standing space: +Y up, -Z forward. Arguments use meters, degrees and seconds. The default `Stage` recentering means the poses you send are absolute.

### Session

| Tool | What it does |
| --- | --- |
| `vr_status` | Startup checks and the state of Steam, SteamVR, ALVR, the headset, the lease and the session clock. |
| `vr_acquire` | Take or renew the SteamVR lease under a name you choose. |
| `vr_release` | Give the lease up so another agent can take it. SteamVR keeps running. |
| `vr_start` | Start Steam if needed, then SteamVR with ALVR and the headset, and return once it streams. |
| `vr_stop` | Shut SteamVR down and disconnect the headset; close Steam only if `vr_start` started it. |
| `vr_restore` | Put back the SteamVR and ALVR setup from before the first `vr_start`. |
| `set_alvr_settings` | Set ALVR session values by path, restarting SteamVR when ALVR needs it. |
| `set_hand_tracking_flavour` | Bare hands as the Index controller (`index`) or as separate skeletal hand trackers (`skeletal_tracker`). |

### Apps

| Tool | What it does |
| --- | --- |
| `launch_app` | Launch a Steam app or shortcut through Steam, or a Steam app straight through its Proton (`via: "proton"`), and wait for its scene. |
| `stop_app` | Stop the scene app, and for a Proton launch its whole process tree. |
| `close_dashboard` | Click the right controller's system button once if SteamVR's dashboard is open, so the game gets focus. |

### Head and playspace

| Tool | What it does |
| --- | --- |
| `set_head_pose` | Set the head pose. |
| `move_head` | Move the head relative to its pose. |
| `animate_head` | Move the head through waypoints over time. |
| `set_proximity` | Tell SteamVR the headset is worn or taken off. |
| `set_playspace` | Set the play area size. |
| `recenter` | Send the play area to ALVR again, which recenters. |

### Controllers

| Tool | What it does |
| --- | --- |
| `set_controller_pose` | Set a controller's pose, in the playspace or relative to the head. |
| `move_controller` | Move a controller relative to its pose. |
| `animate_controller` | Move a controller through waypoints over time. |
| `set_controller_state` | Connect or disconnect a controller. |
| `press_button` | Press and hold an Index button. |
| `release_button` | Release a button. |
| `touch_button` | Touch or untouch a capacitive sensor. |
| `set_trigger` | Set the trigger value. |
| `set_grip` | Set the grip value and force. |
| `set_thumbstick` | Set the thumbstick position, click and touch. |
| `set_trackpad` | Set the trackpad position, force and touch. |

### Hands

| Tool | What it does |
| --- | --- |
| `set_finger_curl` | Curl and splay each finger. |
| `set_hand_mode` | Per hand, hold a controller or be a bare tracked hand. |
| `set_hand_skeleton` | Send raw 26-joint hand data. |
| `set_hand_gesture` | Pose a hand with a named gesture (open, fist, point, pinch, thumbs_up) or custom curls. |
| `run_sequence` | Run timed steps of the head, controller and hand tools, all checked before any runs. |

### Observation

| Tool | What it does |
| --- | --- |
| `screenshot` | PNG from `headset` (what the headset receives, the default), `compositor` (SteamVR's uncompressed scene textures) or `desktop` (the whole desktop through xdg-desktop-portal). |
| `get_tracked_state` | What SteamVR tracks: devices, poses, velocities, skeletons and curls, the scene app. |
| `get_haptics` | Haptic events the app sent. |
| `get_frame_timing` | SteamVR compositor frame timings and ALVR latency statistics over a window. |
| `get_logs` | ALVR events plus the tails of SteamVR's vrserver, vrcompositor and vrmonitor logs. |
| `read_app_log` | Lines of a game's own log files, as `app_logs` configures. |
| `wait_for_log` | Wait for a game log line matching a regex, a `fail_pattern`, or the timeout. |
| `record_audio` | Record the default audio output to a WAV file. |
| `audio_level` | RMS and peak level of the default audio output over a short window. |

Each tool's full description, with every argument and limit, is in its MCP schema (from `src/server.rs`).

### Behavior notes

- `vr_start`, `vr_stop`, `set_alvr_settings`, `set_hand_tracking_flavour`, `launch_app`, `run_sequence` and `wait_for_log` return only when they finish, which can take minutes.
- Waits end with an error that says why: `failed: <signal>` for a known failure, such as vrserver exiting, or `deadline N s`. `wait_for_log` answers `result: "match"`, `"fail"` or `"timeout"`, and fails with `failed: app exited` once the game it watched is gone.
- Times come in two forms. `t_s` is seconds since the MCP started, and `at` is the same instant in UTC (RFC 3339). `vr_status` reports `now_t_s`, `now_at` and `t0_at`, so lines from different tools can be put in order.
- The first connection against a fresh ALVR session always restarts SteamVR once. `vr_start` handles it and reports `restarts`.
- SteamVR opens its dashboard about 10 s after no scene app runs, and a game started under it stays unfocused (OpenXR `VISIBLE`, not `FOCUSED`). `vr_start` reports `dashboard_visible`. `launch_app` closes the dashboard by itself; for a game started another way, call `close_dashboard`.
- SteamVR reserves the Index system click for its dashboard. System touch reaches apps.
- SteamVR gives background OpenVR readers no input until a scene app (SteamVR Home counts) runs, so `get_tracked_state` skeletons read inactive right after `vr_start`.
- `launch_app` with `via: "proton"` takes `exe` relative to the install dir (it must stay inside it) and refuses `STEAM_COMPAT_*`, `SteamAppId` and `SteamGameId` in `env`. The game's Steam launch options do not apply and Steam shows no dialogs. The launcher's output goes to the `run_log` it returns. Like the Steam mode, it needs a streaming headset.
- The first desktop screenshot can bring up xdg-desktop-portal's permission prompt. Approve it once. If nobody answers, the call times out and cancels the request.
- `record_audio` and `audio_level` hear the whole default output, other apps included. WAV files stay in the MCP's private runtime dir until logout.

## What it changes on your system

The first `vr_start` saves a snapshot to `$XDG_STATE_HOME/vr-test-mcp/restore.json` (usually `~/.local/state/vr-test-mcp/`). Later starts never overwrite it. The snapshot holds:

- the `external_drivers` list from `openvrpaths.vrpath`;
- the whole `steamvr.vrsettings`;
- the ALVR session file, and any file ALVR creates next to it;
- the `vrcompositor` wrapper symlink an older ALVR left, if SteamVR had one.

Then `vr_start`:

- registers the pinned ALVR driver in place of any other ALVR driver;
- sets `forcedDriver` to `alvr_server` in `steamvr.vrsettings`;
- clears `blocked_by_safe_mode` for the ALVR driver, and SteamVR's safe-mode blocks on other drivers before each cold start (SteamVR 2.18.4 otherwise stops the launch on a desktop dialog; `vr_status` lists the ones present as `blocked_drivers`);
- unwraps the `vrcompositor` wrapper an older ALVR left, if there is one;
- writes the MCP's ALVR settings, with the headset as a trusted `127.0.0.1` client;
- starts Steam with `steam.env` if it was not running.

It also keeps its own files:

- `$XDG_STATE_HOME/vr-test-mcp/`: `restore.json`, the OpenVR action manifest (`openvr/`), compositor screenshots (`screenshots/`) and Proton launch logs (`runs/`).
- `$XDG_RUNTIME_DIR/vr-test-mcp/`: the lease, the `alvr_dashboard` link and audio recordings (`audio/`). This is a tmpfs that goes away at logout.

### vr_stop

`vr_stop` ends a game that `launch_app via proton` started, disconnects the headset, shuts SteamVR down through ALVR (with signals if ALVR hangs, which takes about 30 s), clears the safe-mode blocks that a signalled stop leaves, and closes Steam if `vr_start` started it. It stops only a SteamVR that runs ALVR; close one you started without ALVR yourself.

It does not put your setup back. After `vr_stop`, SteamVR still has the ALVR driver registered and forced, and ALVR keeps the MCP's settings, so a real headset will not work until you run `vr_restore`.

### vr_restore

`vr_restore` writes back everything in `restore.json` and then deletes it: the driver list, the whole `steamvr.vrsettings`, the ALVR session file, and the `vrcompositor` wrapper. It removes files ALVR created. It refuses while SteamVR runs, so run `vr_stop` first. Anything you changed in SteamVR's settings between the first `vr_start` and `vr_restore` is lost. Because the state is a file, a crashed MCP can still restore. The next `vr_start` takes a fresh snapshot.

## Sharing SteamVR between agents

Several MCP processes (several agent sessions) on one machine share one SteamVR through a lease in `$XDG_RUNTIME_DIR/vr-test-mcp/`.

- Any tool that changes something takes the lease by itself, for 30 minutes unless `vr_acquire` sets `ttl_s` (600 to 14400 s), and renews it once half is left. `vr_acquire` is only needed to choose the name or the ttl.
- While another live MCP holds the lease, the mutating tools disappear from the list and answer `not ready: SteamVR is leased to ...`.
- Read-only tools stay open: `vr_status`, `get_logs`, `get_haptics`, `get_tracked_state`, `screenshot`, `read_app_log`, `wait_for_log`, `get_frame_timing`, `record_audio`, `audio_level`, `vr_acquire` and `vr_release`. An observer has no headset of its own, so for it `get_frame_timing` is unavailable and `screenshot` takes only the `compositor` and `desktop` sources.
- The holder's MCP keeps renewing the lease while it manages a session. The lease ends at `vr_release`, at a `vr_stop` that leaves no SteamVR process behind, at a successful `vr_restore`, or when the holder exits. A lease whose MCP died is free at once; one whose MCP no longer manages a session can be taken over once it expires.
- `vr_status` shows the owner and whether it is this MCP.

## Logging

The server logs through `env_logger` to stderr, and by default only errors are shown. Set `RUST_LOG` in the server's environment for more:

```sh
claude mcp add --scope user -e RUST_LOG=info vr-test-mcp -- /path/to/target/release/vr-test-mcp
```

`warn` adds recoverable problems (skipped events, a reader left running), `info` adds ALVR's client messages, and `debug` is verbose. A one-line note per MCP client connection is always printed.

The logs that matter for a test are available without this: `get_logs` returns ALVR's events and SteamVR's own logs, and `launch_app via proton` writes the launcher's output to its `run_log`.

## Troubleshooting

Start with `vr_status`: each failed check names its reason and the fix.

| Check | Fails when | Fix |
| --- | --- | --- |
| `config` | config.toml does not parse | Fix the file; it is read again at the next `vr_status` or `vr_start` while nothing runs. |
| `steam_native` | No Steam, or Flatpak Steam | Install native Steam or set `steam_dir`. |
| `steamvr_installed` | No `bin/linux64/vrserver` | Install SteamVR (app 250820) and run it once, or set `steamvr_dir`. |
| `alvr_streamer` | No ALVR driver, or another ALVR version | Install the 21.0.0-dev14 streamer and set `streamer_dir`. |
| `alvr_dashboard_absent` | ALVR's dashboard or another vr-test-mcp runs | Close it. |
| `audio_input` | No default audio input | Enable one. |
| `gpu_direct_mode` | Only NVIDIA, or no AMD or Intel GPU | Run SteamVR on AMD or Intel; on hybrids set `steam.env`. |
| `h264_encoder` | No VA-API H.264 encoder on that GPU | Install VA-API drivers with H.264 encoding. |
| `openvr_client` | SteamVR's `libopenvr_api.so` does not load | Reinstall SteamVR. |
| `screenshot_decoder` | OpenH264 fails to start | Rebuild. |
| `desktop_portal` | No xdg-desktop-portal on the session bus | Install it with your desktop's backend (`-kde`, `-gnome`, `-gtk`, `-wlr`). |
| `audio_capture` | No PipeWire | Run PipeWire for the session. |

Common errors at run time:

- `Steam is running without steam.env (...)`: close Steam, and `vr_start` starts it with `steam.env`.
- `SteamVR is running without the ALVR driver` or `SteamVR is running without ALVR; close it yourself`: close the SteamVR you started by hand, then call `vr_start`.
- `port N answers, but no SteamVR of this user runs`: another user's SteamVR or ALVR holds the port. Close it.
- `failed: Steam is waiting for a dialog on the desktop (...)`: Steam stopped a launch on a dialog (`ShowInterstitials` or `ShowLaunchOption`, for example "VR Theater Screen" for an app Steam treats as non-VR). Dismiss it on the desktop and retry.
- `Steam did not take ...`, within 20 s of `vr_start` or `launch_app`: a second Steam client (Flatpak Steam, for example) took over Steam's runtime launch service. Close the other Steam, then restart this one. Two Steam clients under one user also cannot both run a game: the game's steamclient dies about 28 s in with "fatal stalled cross-thread pipe".
- `failed: vrserver exited`: SteamVR crashed during the wait. `get_logs` with `source: "vrserver"` shows why. On a hybrid GPU, check that Steam and the game use the SteamVR GPU.
- `SteamVR wrote no screenshot to ...`: vrserver runs in the Steam Linux Runtime container and cannot write that directory. Keep `$XDG_STATE_HOME` under your home directory.
- `not ready: SteamVR is leased to ...`: another agent holds SteamVR. Wait for it to call `vr_release`, or for its lease to expire once it stops managing a session.
- `vr_restore` refuses: SteamVR still runs. Call `vr_stop` first.

## Limitations

- Restarts are slow. ALVR's TCP stream socket spins at end of stream on loopback (`sockets/src/stream_socket/tcp.rs:137-147`), and the server's receive thread never finishes after the headset drops. So a headset reconnect needs a SteamVR restart, a settings change that needs a restart takes about 80 s, and `vr_stop` usually ends SteamVR with signals. If the MCP exits while the headset streams, vrserver keeps one CPU core busy until SteamVR stops.
- The virtual headset lives inside the MCP process. When the MCP or the agent session running it restarts, a running OpenXR app drops from `FOCUSED` to `SYNCHRONIZED`. The next `vr_start` adopts SteamVR and starts a new headset.
- ALVR 21 registers the left hand tracker's buttons from the right-hand map (`Controller.cpp:112-117`).
- ALVR 21 has one flag per controller for connected and tracked, so a disconnected controller is also untracked.
- Audio tools capture the whole default output, not one game.
- Not covered: Monado, WiVRn, an OpenXR API layer, Windows, Flatpak Steam, headless or CI use, Steam Frame controllers, desktop window capture, eye, face and body tracking, and more than one virtual headset.

## Development

### Bumping the ALVR pin

Change `rev` for every `alvr_*` dependency in `Cargo.toml`, install the matching streamer, and run `cargo test`. Then run the live tests.

### Live tests

The live tests in `tests/live.rs` drive the real Steam, SteamVR and ALVR, so they need a desktop session, a working config and no other agent using SteamVR:

```sh
VR_TEST_GAME_ID=<app id> cargo test --test live -- --ignored --test-threads=1
```

`VR_TEST_GAME_ID` is the Steam app id or non-Steam shortcut game id of a Windows VR game; the menu tests were written against Beat Saber. The `xr_probe` example (`cargo build --release --example xr_probe`) is a native OpenXR app that the live tests use to check inputs and haptics.

The live tests leave SteamVR running for the next one. Only `live_zz_stop_and_restore` stops it and restores the setup. After running a single live test, run that one too, or call `vr_stop` and `vr_restore`.

### Manual checks through the MCP

`cargo test` does not cover these. Run them through the MCP from a desktop session while no other agent uses SteamVR. Each line gives the call and what passing looks like.

1. Clocks: `vr_status` shows `now_t_s`, `now_at` and `t0_at`. After `vr_start`, a `get_logs` vrserver line and an ALVR line from the same second have `t_s` within 1 s. A `get_haptics` event fired by `xr_probe` has `t_s` within 0.2 s of the probe's own log line.
2. Cold start: a cold `vr_start`, and a `set_alvr_settings` that restarts SteamVR, never fail with `failed: vrserver exited`.
3. Lease: start two MCP sessions. Session A calls `set_head_pose`. Session B's tool list drops the mutating tools within 2 s, `vr_status.state.owner.mine` is false in B, and B's `vr_start` answers `not ready: SteamVR is leased to mcp-<A pid>`. A runs `vr_release`, and B's mutating tools come back within 2 s. Kill A with SIGKILL while it holds the lease, and B can claim at once. While A runs a session and calls only read-only tools for over 30 minutes, A's lease stays renewed. While A restarts SteamVR, B's `get_tracked_state` fails or answers within 5 s and B keeps working afterwards. (In Claude Code every tool stays listed, so check B's answers instead of its tool list.)
4. Safe mode: `vr_start`, then kill vrmonitor with SIGKILL. Call `vr_status` until `blocked_drivers` lists the blocked drivers. `vr_stop`, and `blocked_drivers` is back to the snapshot's state. `vr_start` streams.
5. App logs: add `[app_logs]` for a Unity Proton game and `launch_app` it. `read_app_log` returns lines from the new `Player.log` only. `wait_for_log {pattern: "<a line the game prints at its menu>", fail_pattern: "Exception"}` answers `match` within the game's load time.
6. Proximity: the headset reports worn at each stream start (SteamVR 2.18 needs that to focus OpenXR apps). With an OpenXR app running, `set_proximity {on_head: false}` makes `get_tracked_state` show the HMD's `head_proximity: false`, and the app reports focus loss or idle. `on_head: true` reverses it. Call `get_tracked_state` right after `set_proximity`, both after `vr_start` and after `launch_app`: it shows the new value.
7. Frame timing: with a game in VR, `get_frame_timing {window_s: 5}` returns a non-null `compositor` with `fps` near the stream fps, a non-null `alvr`, and empty `notes`.
8. Compositor screenshot: `screenshot {source: "compositor", eye: "left"}` returns a PNG at the app's eye render size, sharper than `source: "headset"`.
9. Proton launch: `launch_app {app_id, via: "proton", exe: "<game exe>", args: ["<arg>"], env: {"PROTON_LOG": "1"}}` reaches a running scene without a Steam dialog. The game sees the arg, and `run_log` has Proton's output. `stop_app` returns, and afterwards `/proc` has no process with that `STEAM_COMPAT_DATA_PATH` and app id. Launch again, then `vr_stop`: its `stopped_app` names the game, and a second Proton launch of the same app reaches its scene. Run it once with Proton Experimental and once with a custom compatibility tool.
10. Desktop screenshot: `screenshot {source: "desktop"}` returns the current desktop. A permission prompt may appear on the first call; the second call shows none.
11. Audio: play a test tone or the game's menu music. `audio_level {window_s: 1}` reads above -60 dBFS on both channels. Turn the head 90 degrees with `move_head {yaw: 90}`, and the L/R balance of a spatialized sound shifts. `record_audio {seconds: 3}` writes a WAV that plays back the same sound. Repeat on a mono and a 5.1 default sink.

## Third-party files

- `vendor/openvr/openvr_capi.h` is Valve's OpenVR SDK 2.15.6 C header, as ALVR's `openvr` submodule pins it, under the BSD 3-clause license in `vendor/openvr/LICENSE`.
- `examples/xr_probe.rs` and its shaders in `examples/xr_probe/` are adapted from the Vulkan example of [openxrs](https://github.com/Ralith/openxrs) 0.22, by Benjamin Saunders and the openxrs contributors, Copyright (c) 2020 The openxrs Developers, used under MIT OR Apache-2.0. The MIT permission notice that covers it is the same text as this repository's [LICENSE-MIT](LICENSE-MIT); the changes are listed at the top of `examples/xr_probe.rs`.
- `tests/fixtures/red_blue.h264` is a synthetic one-frame x264 encode; the command that makes it is next to the test in `src/headset.rs`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

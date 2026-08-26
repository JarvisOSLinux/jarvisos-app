# CLAUDE.md — jarvisos-app

## What This Is

Desktop UI for the JARVIS daemon. ChatGPT-style chat interface built with
Rust + Tauri 2 + HTML/CSS/JS. Connects to the JARVIS Python daemon over a
bidirectional IPC channel using newline-delimited JSON — a Unix socket on
Linux/macOS, loopback TCP plus a token handshake on Windows.

## Role in the JARVIS Ecosystem

jarvisos-app is the graphical alternative to Project-JARVIS's Textual TUI.
Both connect to the same daemon over the same IPC protocol. The TUI is the
primary workspace; the Tauri app is for desktop Linux users who prefer a GUI.

## Tech Stack

- Rust + Tauri 2 (native backend, WebKitGTK frontend on Linux)
- HTML/CSS/JS (no framework, no bundler)
- serde / serde_json for JSON IPC and Tauri command payloads
- WebKitGTK 4.1 (Linux), WebView2 (Windows), WKWebView (macOS)

## Architecture

```
src-tauri/
├── src/
│   ├── main.rs       Tauri entry point
│   ├── lib.rs        #[tauri::command] handlers + IPC poll thread
│   ├── ipc.rs        IPC client with auto-reconnect (background thread)
│   └── widgets.rs    Widget manifest discovery (bundled + installed scopes)
├── resources/
│   ├── wake_chime.wav      Bundled default wake-word sound (one-time override)
│   └── widgets/
│       └── wake-word-orb/  Bundled widget: #16 reimplemented on #18
├── capabilities/
│   ├── default.json  Main window: core:default, dialog:default, ...
│   └── widget.json   Widget windows (`widget-*`): core:event:allow-listen only
├── Cargo.toml
├── tauri.conf.json   Window config, withGlobalTauri: true, bundle.resources
└── build.rs          tauri-build

src/
├── index.html        App shell (header, chat-view, input-bar)
├── styles.css        Dark navy + cyan theme
└── main.js           IPC event listeners, message rendering

python/
├── ipc_server.py          Async Unix socket server (daemon side)
└── main_integration.py    Integration guide for wiring into JARVIS daemon
```

### IPC Protocol

Socket: `<data_dir>/jarvis.sock`, mirroring the daemon's per-OS data_dir
(Linux `~/.local/share/jarvis`, macOS `~/Library/Application Support/jarvis`,
Windows `%LOCALAPPDATA%\jarvis`). Override with `JARVIS_SOCKET` env var.
Newline-delimited JSON.

On Windows there is no Unix socket: the daemon publishes an ephemeral
loopback-TCP port in `<socket>.port` and a per-startup auth token in
`<socket>.token`, and the app must send that token as the connection's
first line or the daemon rejects the peer (`connect_tcp_with_token` in
`src-tauri/src/ipc.rs` — portable and round-tripped by tests on every
platform, not just compiled for Windows).

**Client -> Daemon**: `message`, `start_listening`, `stop_listening`,
                      `stop_stream`, `confirmation_response`, `ping`,
                      `list_sessions`, `create_session`, `switch_session`,
                      `rename_session`, `delete_session`,
                      `set_wake_chime_path`, `reset_wake_chime_path`,
                      `get_settings`, `set_confirmation_mode`,
                      `list_providers`, `add_provider`, `edit_provider`,
                      `remove_provider`, `list_clients`, `shutdown_request`
**Daemon -> Client**: `state`, `response` (streaming), `wake_word_detected`,
                      `confirmation_request`, `error`, `session_list`,
                      `session_switched`, `session_error`,
                      `config_updated`, `config_error`, `settings`,
                      `provider_list`, `provider_error`, `client_list`,
                      `DAEMON_SHUTDOWN`

Session CRUD (`Project-JARVIS`'s `jarvis/runtime/io.py`) is a thin wrapper
over the existing `SessionManager` (`new_session`/`list`/`switch`/`rename`/
`delete`) -- no separate storage on the GUI socket side. `switch_session`
returns the session plus its `conversation_log` history as
`{role, content, timestamp}` messages so a client can render it immediately.
Session mutations (create/switch/rename/delete) broadcast to every connected
GUI client, since there's one shared "current session" pointer on the
daemon; `list_sessions` and errors reply to the requester only.

States: `idle`, `woken`, `capturing`, `listening`, `processing`, `speaking`, `offline`.
`woken`/`capturing`/`processing`/`speaking` come from the daemon's formal
voice/response state machine (`Project-JARVIS`#141); `listening` is the
separate manual mic-toggle state; `offline` is client-side only (never sent
by the daemon). The daemon's `state` message can also carry a `meta` object
(currently just a discard reason) -- not yet consumed here.

### Wake chime one-time bootstrap (Project-JARVIS#139)

On first run only, jarvis-app overrides the daemon's `WAKE_CHIME_PATH` to
its own bundled sound (`resources/wake_chime.wav`, a distinct three-note
arpeggio from `Project-JARVIS`'s own bundled default) — proving an external
project can actually customize the daemon's chime, not just that the config
key theoretically exists. Implementation (`lib.rs`):

- `pending_wake_chime_bootstrap()` checks a marker file in the app's own
  data dir (`app_data_dir()/.wake_chime_bootstrapped`, distinct from the
  daemon's `jarvis_data_dir()` used for socket discovery) at startup. If it
  exists, nothing happens -- this really does run only once, ever.
- If absent, the bundled resource path is resolved and `set_wake_chime_path`
  is sent on every `Connected` event (retried across reconnects) until a
  `config_updated` response for `WAKE_CHIME_PATH` confirms success, at which
  point the marker is written and the poll loop stops sending it.
- A `config_error` response is logged and left to retry on the next
  reconnect/app launch -- a daemon that's merely offline on first launch
  shouldn't silently skip the bootstrap forever.
- After this one-time bootstrap, `jarvisos-app`#12's settings panel is the
  ongoing, user-facing surface for changing the wake sound -- this code
  never touches `WAKE_CHIME_PATH` again.

### Settings panel (Project-JARVIS#12)

A centered modal (`#settings-btn` in the header) with two tabs:

- **General** -- voice-activation toggle (an alias for the same
  `toggle_listening` command the mic button uses), confirmation mode
  (`smart`/`ask_all`/`allow_all`), wake sound (native file picker via
  `tauri-plugin-dialog`'s `pick_wake_chime_file`, or "Restore default" via
  `reset_wake_chime_path`), and a read-only socket path for troubleshooting
  (`get_connection_info` -- local only, never round-trips to the daemon).
- **Providers** -- list/add/edit/remove backed by the daemon's
  `jarvis/core/providers.py` CRUD (shared with the TUI's own config modal).
  **Provider changes only take effect on the daemon's next restart** -- there
  is no live `ProviderPool` reload, matching the TUI's own limitation; the
  panel says so rather than implying an instant effect.

`ConfigUpdated`/`ConfigError` (`ipc.rs`) already existed for the wake-chime
bootstrap above, but were previously consumed only internally by the poll
loop -- never forwarded to the frontend. They're now also emitted as
`ipc-config-updated`/`ipc-config-error` unconditionally, so the settings
panel can reflect a `CONFIRMATION_MODE` or `WAKE_CHIME_PATH` write from
*any* source (including another connected client), not just its own.

### Graceful daemon shutdown (Project-JARVIS#146 / jarvisos-app#17)

Three entry points converge on the same confirmation modal (`#shutdown-modal`
in `index.html`): the settings panel's General tab ("Shut Down JARVIS..."),
the tray menu item of the same name (emits `ipc-open-shutdown-modal`, shown
via `show_main_window()` first since the window may be hidden), and
(implicitly) any future quit-sequence hook. None of them send
`shutdown_request` directly:

1. Opening the modal immediately calls `list_clients` and shows a loading
   placeholder until `client_list` replies -- the user always sees who else
   is currently attached before being asked to confirm.
2. Only the modal's own "Shut Down" button sends `request_daemon_shutdown`.
   Cancelling (or clicking the backdrop) never contacts the daemon at all.
   There is no daemon-enforced confirmation gate on `shutdown_request` itself
   (`Project-JARVIS`#146's own design) -- this modal *is* the client-side
   check the protocol expects every well-behaved client to do.
3. `DAEMON_SHUTDOWN` (from this client's own request *or* another client's)
   closes the modal if it's open, appends a system-message line into the
   current chat ("JARVIS shut down at HH:MM. Last state: ...") as a
   lightweight, client-local stand-in for "recorded into session history"
   (no round-trip write to a daemon that's already tearing itself down), and
   proactively runs the same `setConnected(false)`/`setStatus('offline')`
   treatment `ipc-disconnected` uses -- the real disconnect follows moments
   later regardless, this just avoids a lag between the two.
4. **Self-quit on shutdown is opt-in** (`#settings-quit-on-shutdown`
   checkbox, off by default), persisted to a small local
   `app_data_dir()/local_settings.json` (`get_quit_on_daemon_shutdown` /
   `set_quit_on_daemon_shutdown`) -- deliberately separate from the daemon's
   own settings surface above, since this is a jarvis-app-only preference
   that means nothing to any other connected client.

**Scoped out of this pass**: the issue's "if jarvis-app spawned the daemon
itself" criterion. jarvis-app has no daemon-spawning code at all today --
it only ever connects to an existing socket -- so there is no "if" case to
handle yet; that's a separate, substantial feature (finding/launching the
`jarvis` binary, tracking its lifecycle) left for a future pass.

### Widget plugin system (jarvisos-app#18 / #16)

A widget is a directory containing `manifest.json` (`id`, `name`,
`description`, `icon`, `entry`, `trustStatus`, `subscribedEvents`) plus its
own `index.html`/CSS/JS -- consistent with this app's own "no framework, no
bundler" approach, and mirroring how `dmcp`/`mcp-registry` describe an MCP
server. `src-tauri/src/widgets.rs` discovers widgets from two scopes, scanned
identically and tagged with a `source` field: **bundled**
(`resources/widgets/<id>/`, shipped with the app -- `tauri.conf.json`'s
`bundle.resources` includes `resources/widgets/**/*` so the directory is
packaged recursively, unlike the flat `resources/*` glob used for
`wake_chime.wav`) and **installed** (`app_data_dir()/widgets/<id>/`,
user-added, not built yet -- a dedicated default/community widget registry
repo mirroring `mcp-registry` is a separate, deliberate step left for later).
A malformed widget (missing `id`/`name`/`entry`, invalid JSON) is silently
skipped rather than breaking discovery for every other widget.

Each enabled widget gets its own always-on-top, undecorated, transparent,
non-resizable window (`lib.rs`'s `open_widget_window`), labeled
`widget-<id>` and served over a dedicated `widget://<id>/<path>` URI scheme
(`widget_protocol_response`) rather than the app's own bundled `tauri://`
asset protocol -- installed widgets live at a runtime path unknown at build
time, so they can't be part of `frontendDist`. Registering a custom scheme
also means every response gets an explicit, restrictive
Content-Security-Policy header (`default-src 'self'; script-src 'self';
style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'none';
frame-src 'none'; object-src 'none';`) applied directly in the handler,
rather than relying on `on_web_resource_request` (documented as "currently
only implemented for the `tauri` URI protocol", so it wouldn't cover
widgets at all). The handler rejects any request path containing an empty
or `..` segment, then canonicalizes both the serving directory and the
resolved file and requires the file to remain a descendant of the
directory -- belt-and-braces against a crafted path escaping the widget's
own files.

Every widget window is covered by `capabilities/widget.json`
(`"windows": ["widget-*"]`, granting only `core:event:allow-listen`) --
deliberately narrower than the main window's `capabilities/default.json`
(`core:default`, `dialog:default`, etc.): a widget only ever needs to
render its own UI and listen for the same `ipc-state`/`ipc-wake`/
`ipc-connected`/`ipc-disconnected` events the main window already emits via
`AppHandle::emit` (which reaches every window, main or widget, with no
extra wiring). `#16`'s wake-word orb (`resources/widgets/wake-word-orb/`)
is the first (and, for now, only) bundled widget built on this system --
not a separate implementation, `trustStatus: "verified"` since it ships
with the app itself.

Per-widget state lives in the existing local-only `LocalSettings`/
`local_settings.json` (never round-tripped to the daemon, since these are
jarvis-app-only client preferences): `enabled_widgets` (which widgets open
at startup, via `reopen_enabled_widgets`), `widget_positions` (last dragged
position, written on every `WindowEvent::Moved` for a `widget-*`-labeled
window -- no batching/throttling, movement is infrequent enough that it
doesn't need it), and `widget_appearance_overrides` (an id -> local
directory map letting a user point a widget at their own HTML/CSS/JS bundle
instead of its bundled/installed one; `pick_widget_appearance_folder` uses
`tauri-plugin-dialog`'s `.pick_folder()`, already a dependency from #12's
wake-sound picker, and requires the picked folder to contain its own
`manifest.json` before accepting it).

The Settings panel's **Widgets** tab (`list_widgets`/`set_widget_enabled`/
`pick_widget_appearance_folder`/`reset_widget_appearance`) lists every
discovered widget with its `trustStatus` badge, an enable/disable toggle,
and appearance-override controls.

### Tauri Command / Event Mapping

| Tauri command (JS → Rust)       | Tauri event (Rust → JS)  |
|---------------------------------|--------------------------|
| `send_message`                  | `ipc-connected`          |
| `toggle_listening`              | `ipc-disconnected`       |
| `stop_stream`                   | `ipc-state` (string)     |
| `send_confirmation_response`    | `ipc-chunk` `{content, done}` |
| `list_confirmations`            | `ipc-wake`               |
| `approve_confirmation`          | `ipc-confirm` `{id, tool_names}` |
| `deny_confirmation`             | `ipc-confirmation-list` `[{id, tool_names, created_at}]` |
| `approve_all_confirmations`     |                          |
| `get_settings`                  | `ipc-settings` `{confirmation_mode, wake_chime_path}` |
| `set_confirmation_mode`         | `ipc-config-updated` `{key, value}` |
| `reset_wake_chime_path`         | `ipc-config-error` `{key, message}` |
| `list_providers` / `add_provider` / `edit_provider` / `remove_provider` | `ipc-provider-list` `[{name, type, model, ...}]` |
| `pick_wake_chime_file` (native file dialog, no daemon round-trip) | `ipc-provider-error` (string) |
| `get_connection_info` (local socket path, no daemon round-trip) | |
| `list_clients` / `request_daemon_shutdown` | `ipc-client-list` `[label, ...]` |
| `get_quit_on_daemon_shutdown` / `set_quit_on_daemon_shutdown` (local file, no daemon round-trip) | `ipc-daemon-shutdown` `{state, goals, session_id, timestamp}` |
| `list_widgets` / `set_widget_enabled` / `pick_widget_appearance_folder` / `reset_widget_appearance` (all local, no daemon round-trip) | `ipc-open-shutdown-modal` (tray menu entry point, no payload) |

## Build

### Prerequisites

Arch/CachyOS: `sudo pacman -S webkit2gtk-4.1 gtk3 rust`
Fedora:       `sudo dnf install webkit2gtk4.1-devel gtk3-devel rust cargo`

Install Tauri CLI: `cargo install tauri-cli`

### Compile

```bash
# Development (hot-reload frontend, native backend)
cargo tauri dev

# Release build
cargo tauri build
# Binary: src-tauri/target/release/jarvis-ui
```

## Theme

Dark navy (`#0a0e1a`) + JARVIS cyan (`#00c8ff`). Monospace font: Hack/JetBrains Mono.

## Conventions

- `cargo fmt` + `cargo clippy` clean before pushing
- Commit messages: imperative mood
- No comments explaining what code does; only non-obvious WHY

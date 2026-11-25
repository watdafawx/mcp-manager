# MCP Manager (`mcpm`): spec

One native Rust app that manages every MCP server on this machine for every AI harness.
It installs itself as the only MCP entry in each harness, proxies the servers it took over,
and can put every harness config back exactly as it was.

Successor of the `mcp-relay` T3 Patcher plugin. Reference implementation (Node, tested) to port from:
[`plugins/mcp-relay`](https://github.com/watdafawx/t3-patcher/tree/main/plugins/mcp-relay) (`discovery.cjs`, `autoconnect.cjs`, `relay-pool.cjs`,
`relay-clients.cjs`, `relay-http.cjs`, tests in `test/`). Read them; keep their behaviour unless this spec says otherwise.
UX inspiration: https://github.com/xingkongliang/skills-manager (one library, per-agent pages, badges per agent).

## Goals

- Low resources: daemon idle under ~10 MB RAM, no web tech anywhere. GUI only uses memory while open.
- Never lose a user's config. Every write is snapshotted first; restore is always possible.
- Coexists with the old `mcp-relay` plugin (different port, different entry name).

## Layout

Cargo workspace, edition 2024, MSRV = installed toolchain (1.99).

| Crate | Kind | Owns |
|---|---|---|
| `crates/mcpm-core` | lib | types, manager config store, harness registry, config parsing/editing (JSON, JSONC, TOML), discovery, snapshots, takeover, restore. No tokio, no network. |
| `crates/mcpm-daemon` | lib | tokio runtime: child pool (stdio + Streamable HTTP clients), MCP server endpoint, meta-tools, control API, stdio bridge. Depends on core. |
| `crates/mcpm-gui` | lib | eframe/egui window (glow backend). Talks to the daemon only through the control API. Depends on core (types only). |
| `crates/mcpm` | bin `mcpm.exe` | CLI entry, subcommands below. Depends on all three. |

Single exe. Built with `#![windows_subsystem = "windows"]`; CLI subcommands call `AttachConsole(ATTACH_PARENT_PROCESS)` so output shows in a terminal, and `mcpm daemon` never shows a console.

## Paths

- State dir: `%APPDATA%\mcpm\` (`$XDG_CONFIG_HOME/mcpm` or `~/.config/mcpm` elsewhere). Override with env `MCPM_HOME` (tests use it).
  - `config.json`: manager config (`ManagerConfig`).
  - `token`: random 32-byte hex control token, created on first run, file only readable by the user.
  - `snapshots/<snapshot-id>/manifest.json` + `files/<n>.bak` raw bytes.
  - `daemon.json`: `{ pid, port, startedAt }` written by the running daemon, removed on clean exit.
  - `logs/daemon.log` (rolling, max 2 x 1 MB).
  - `updates/`: a downloaded `mcpm.exe` waiting to be installed; emptied on the next daemon start.
- Default port **39494** on `127.0.0.1` only (the old relay uses 39393).

## Core types (`mcpm-core::model`)

```rust
pub struct ServerConfig {
    pub id: String,                 // ^[a-z0-9][a-z0-9_-]{0,47}$ (case-insensitive)
    pub name: String,
    pub transport: Transport,       // Stdio { command, args, env, cwd } | Http { url, headers }
    pub enabled: bool,
    pub mode: InstanceMode,         // Shared (one child for all callers) | Session (one child per MCP session)
    pub pinned_tools: Vec<String>,  // exposed directly as `<id>__<tool>` besides the meta-tools
    pub harnesses: Vec<String>,     // empty = exposed to every harness
    pub origin: Option<Origin>,     // where it was imported from (harness id, config path, entry name, scope)
}
pub struct ManagerConfig { pub version: u32, pub port: u16, pub servers: Vec<ServerConfig>, pub autostart: bool, pub auto_update: bool /* default true */ }
```

Unlike the relay, taken-over servers are **copied** into `config.json` with their env/headers, because takeover removes them from the harness file.
`config.json` lives in the user profile; on Windows set its ACL to the current user only. Secrets are never logged and are masked (`••••`) in the GUI and in meta-tool output.

## Harnesses

`HarnessDef { id, label, files: Vec<HarnessFile> }`, `HarnessFile { path, format: Json|Jsonc|Toml, container: key path, scope: Global|Project, entry_style }`.
Detected = file exists, or the app's install dir exists. v1 global files:

| id | file (Windows) | container | manager entry written |
|---|---|---|---|
| claude | `~/.claude.json` | `mcpServers` (also read-only list of `projects.*.mcpServers`) | `{ "type": "http", "url": U }` |
| claude-desktop | `%APPDATA%\Claude\claude_desktop_config.json` | `mcpServers` | stdio bridge: `{ "command": "<mcpm.exe>", "args": ["bridge", "--harness", "claude-desktop"] }` |
| codex | `~/.codex/config.toml` | `[mcp_servers.<name>]` | `url = U` (streamable http) |
| opencode | `~/.config/opencode/opencode.jsonc` or `.json` | `mcp` | `{ "type": "remote", "url": U, "enabled": true }` |
| cursor | `~/.cursor/mcp.json` | `mcpServers` | `{ "url": U }` |
| vscode | `%APPDATA%\Code\User\mcp.json` | `servers` | `{ "type": "http", "url": U }` |
| antigravity | `~/.gemini/antigravity/mcp_config.json` (+ the other two paths in `discovery.cjs`) | `mcpServers` | `{ "serverUrl": U }` (check autoconnect.cjs for exact key) |
| gemini-cli | `~/.gemini/settings.json` | `mcpServers` | `{ "httpUrl": U }` |
| windsurf | `~/.codeium/windsurf/mcp_config.json` | `mcpServers` | `{ "serverUrl": U }` |

`U = http://127.0.0.1:<port>/mcp/<harness-id>`. The path segment tells the daemon which harness is calling (more reliable than clientInfo; fall back to clientInfo like `detectHarness` in relay-http.cjs for plain `/mcp`).
Manager entry name in every harness: `mcpm`. Entry formats: copy from `autoconnect.cjs` `buildEntry`/`codexBlock` where it already handles a harness; it is tested against real apps.
Project scope (`.mcp.json`, `.cursor/mcp.json`, `.vscode/mcp.json`, Claude `projects.*`) is listed read-only in v1, never rewritten.

Editing must preserve the rest of the file: JSONC comments/format (text-splice edits like `autoconnect.cjs`, or a CST crate such as `jsonc-parser`), TOML via `toml_edit`. `~/.claude.json` is large and rewritten by Claude Code constantly: re-read immediately before writing, write atomically (temp file + rename), retry once on a changed mtime.

## Snapshots, takeover, restore (core)

Snapshot = for each file touched: original path, `existed`, raw bytes, sha256 before, sha256 after our write, timestamp, operation (`takeover` / `connect` / `edit`). Snapshots are immutable and kept until the user deletes them (GUI lists them).

**Takeover(harness)** (also `takeover_all`):
1. Snapshot the file.
2. Parse MCP entries from the container. Skip our own `mcpm` entry and the old `t3-relay` entry.
3. Import each into `ManagerConfig` (`origin` set). Same command+args/url already present = merge `harnesses` instead of duplicating (see `dedupeCandidates` in discovery.cjs). Imported server's `harnesses` = `[that harness]` so every harness keeps seeing exactly what it saw before.
4. Remove those entries from the harness file, add the `mcpm` entry. One atomic write.
5. Record post-write sha256 in the snapshot.

Dry run (`plan_takeover`) returns the diff (entries to import, entries to remove, final file text) without writing; GUI and CLI show it before applying.

**Restore(snapshot or harness)**:
- If the current file sha256 == the snapshot's post-write sha256 (nobody touched it): write the original bytes back (or delete the file if it did not exist).
- Else (the harness changed the file since): surgical restore: remove the `mcpm` entry, re-add every original MCP entry from the snapshot bytes that is missing, keep everything else as it is now. Report it as `merged`.
- Imported servers whose `origin` harness was restored are disabled for that harness (removed from `harnesses`; server deleted from config if no harness left), unless the user chose "keep in manager".
- `restore_all` restores every harness taken over; `mcpm uninstall` = `restore_all` + remove autostart + stop daemon.

## Daemon

- One process, tokio multi-thread runtime with 2 worker threads. MCP via the `rmcp` crate (official Rust SDK; check crates.io for current version and its client + streamable-http server features). Hand-roll only what rmcp lacks.
- Child pool: port `relay-pool.cjs` + `relay-clients.cjs`: lazy start on first use, Shared vs Session instances, idle Session instances killed after 15 min, crash = mark errored and restart on next call, call log ring buffer (100 entries), tool list cache invalidated on `notifications/tools/list_changed`. On Windows spawn children with `CREATE_NO_WINDOW` and in a Job Object with `KILL_ON_JOB_CLOSE` so they die with the daemon. `needsShell` logic for `.cmd`/`.bat` (npx etc.).
- Endpoint: Streamable HTTP at `/mcp` and `/mcp/<harness>`. Tools exposed to a caller = meta-tools + pinned tools of servers exposed to that harness:
  - `mcpm_list_servers`, `mcpm_list_tools { server?, query? }`, `mcpm_call { server, tool, arguments }`, `mcpm_reload_server { server }` (same contract as the relay's `relay_*` tools).
  - pinned: `<serverid>__<tool>` with the child's schema.
  - Config changes send `notifications/tools/list_changed` to connected sessions.
- Control API (JSON over the same listener, `/api/*`, requires header `Authorization: Bearer <token>`): status, list/add/update/remove/enable/disable/restart servers, list tools of a server, call log, harness list + status, plan/apply takeover, list snapshots, restore, shutdown. The GUI and CLI use this; core operations that touch files run inside the daemon so there is a single writer. If the daemon is not running, the CLI runs core operations directly.
- `mcpm bridge --harness <id>`: stdio <-> HTTP bridge for stdio-only harnesses; starts the daemon if not running.
- Autostart: HKCU `Software\Microsoft\Windows\CurrentVersion\Run` value `mcpm` = `"<exe>" daemon`. No admin.
- Single instance: if `daemon.json` points at a live pid answering `/api/status`, a second `mcpm daemon` exits 0.
- `mcpm daemon --no-tray` skips the notification-area icon; `--wait-pid <pid>` (hidden, used by updates) waits for that process to exit before starting.

### Tray icon

The daemon owns a notification-area icon, on its own thread with a Win32 message loop (`Shell_NotifyIconW`, no extra crates). It is skipped with `--no-tray`, and when the session has no visible desktop (service, ssh). The icon is generated (`tools/gen_icon.py` -> `assets/`), embedded as the exe's icon resource and used for the window.

- Tooltip and first menu line: `Running · N servers · port P`.
- Menu: status line (disabled) · **Open MCP Manager** (focuses the open window, else starts `<exe> gui`) · **Restart all servers** · **Check for updates** · **Install update x.y.z** (only when one is known; "Updating to x.y.z…" while it runs) · **Start with Windows** (checkbox, the same Run key as `mcpm autostart`) · **Quit** (stops the daemon cleanly).
- Double-click opens the window. The tooltip refreshes every 5 s; the icon comes back after an Explorer restart.

### Self-update

- Source: GitHub `releases/latest` of `watdafawx/mcp-manager` (no token, `User-Agent: mcpm/<version>`). Drafts and pre-releases are ignored. A release counts when its tag is a higher semver than the running version and it carries `mcpm.exe`.
- Schedule: first check ~20 s after the daemon starts, then every 6 h. Setting `autoUpdate` (config, default `true`): on = install when no tool call is in flight; off = tray, window and `mcpm update --check` only report it.
- Download to `<state>/updates/`; size and sha256 must match the release (`mcpm.exe.sha256` asset, else GitHub's `sha256:` digest). Without a checksum the update is refused.
- Swap: the running exe is renamed to `mcpm.exe.old`, the new file moved in (a failure puts the old one back), a new `mcpm daemon --wait-pid <old pid>` is started with the same flags, the old daemon stops. The new daemon deletes `mcpm.exe.old` and `updates/` and restarts the child servers lazily (the old daemon's job object killed the old children).
- A running window sees `/api/status` report a new version: banner "Updated to x.y.z" with a Restart window button. A daemon older than the window offers Restart daemon.
- Development builds (exe under a cargo `target\debug|release` folder, or `MCPM_NO_UPDATE` set) never self-update. Hidden `MCPM_UPDATE_URL` replaces the GitHub API base for tests (docs/release.md).
- Control API: `GET /api/update` (state: current, latest, available, phase, notes, lastCheck, lastError, autoUpdate, waitingForIdle) · `PUT /api/update {autoUpdate}` · `POST /api/update/check` · `POST /api/update/install` (starts it, returns at once). `501` when updates are off. `/api/status` also carries `autoUpdate` and `updateAvailable`.

## CLI (`mcpm`)

`daemon` · `gui` (default when run with no args, e.g. double-click) · `status` · `harnesses` · `takeover [<harness>|--all] [--dry-run]` · `restore [<harness>|--all|--snapshot <id>]` · `snapshots` · `servers [list|add|remove|enable|disable]` · `bridge --harness <id>` · `autostart on|off` · `update [--check]` · `uninstall`.

## GUI (egui)

Opens with `mcpm gui`; starts the daemon if needed; closing the window leaves the daemon running. Pages (left nav):
- **Servers**: one row per server: name, transport, status dot (idle/running/error), tool count, per-harness badges (click toggles exposure, like skills-manager), enable switch, restart. Detail pane: edit form (command/args/env with masked values, or url/headers), mode, pinned tools picker from the live tool list, recent calls.
- **Harnesses**: one card per detected harness: config path, state (not connected / connected alongside / taken over), MCP entries found, buttons Take over (shows dry-run diff first) / Restore.
- **Snapshots**: list, view diff vs current file, restore, delete.
- **Settings**: port, autostart, updates (versions, automatic toggle, Check now, Install, last check and error), open state dir, uninstall everything.
A second `mcpm gui` brings the open window to the front instead of opening another.
Theme: follow system dark/light, no hard-coded garish colours.

## Testing

- core: unit tests for every parser/editor (JSONC with comments and trailing commas, TOML tables, Claude projects map), takeover plan, restore both paths (exact and merged), dedupe. Use `MCPM_HOME` + temp dirs, never real user files. Port the cases in `plugins/mcp-relay/test/*.test.ts`.
- daemon: integration test with a tiny fake stdio MCP server (a test bin in the crate) exercising list/call/pinned/reload/session isolation and the control API.
- GUI: checked by hand on a separate Windows machine.

## Building

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release -p mcpm      # target/release/mcpm.exe
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy and the tests on Windows, and the core tests on Linux. Releases: `docs/release.md`.

Tests never touch real harness files: they run against temp dirs (`MCPM_HOME` plus a temp `Core { home, appdata, state_dir }`).

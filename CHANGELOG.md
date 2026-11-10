# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/).

[中文](./CHANGELOG.zh-CN.md) · **English**

## [2.5.0] - 2026-11-10

### Added

- Toolbox: a new title-bar entry (enabled once a host is connected) that gathers firewall, port, public-key and Docker management in one modal
  - Firewall: detects ufw / firewalld / iptables, shows status and rules, and supports adding / removing rules and enabling / disabling the firewall (the current SSH port is allowed automatically when toggling)
  - Ports: probes listening ports via ss → netstat → lsof and enriches rows with process, user and Docker container names (including docker-proxy published ports); the root view shows ports of all users
  - Public keys: appends a local key to the target user's authorized_keys, deduplicated and keeping the original file
  - Docker: container and image lists, start / stop / restart / remove, image pull / tag / remove; container log copy and local download; an interactive terminal inside a container (dedicated SSH channel + PTY); image tar import / export (remote paths, or local files / folders relaying over SFTP); a visual editor for `data-root` and `registry-mirrors` in daemon.json
  - Root operations elevate on demand: passwordless sudo is tried first, then a password prompt; the password only travels over stdin and is never logged
- The save preview on the Docker config page is now an editable daemon.json editor: other keys such as `log-driver` can be added or removed directly and the edited content is what gets saved. Editing the form afterwards syncs the matching keys, and the edit can be reverted to the generated content. The backend validates the JSON structure and the `data-root` / `registry-mirrors` values before saving
- Host cards now show the group they belong to

### Fixed

- Selecting text in the new-connection modal and releasing the mouse over the mask no longer closes the modal: only clicks that start on the mask count

## [2.4.2] - 2026-11-07

### Fixed

- The docker memory / docker CPU presets in the "Run command…" panel disappeared: the presets existed only in the frontend defaults, and any settings save made the backend write an empty array based on its own defaults, which then overwrote the frontend presets on every launch. The first launch after upgrading re-seeds them when the list is empty (existing custom commands are kept), and later deliberate deletions stay deleted
- Metrics occasionally fell back to 0 or "waiting for collection" when a collection round failed: a failed round now keeps the previous frame, and a missing or empty section (CPU, load, memory, network, disk, processes) reuses that section's last valid value

### Changed

- The disk mount-path table now refreshes every 30 rounds (about 60 seconds at the default 2-second interval), cutting remote df overhead; the first frame is still collected immediately

## [2.4.1] - 2026-11-07

### Fixed

- Saved passwords on Windows and Linux did not survive a restart (and so had to be re-entered after every upgrade): the keyring dependency did not enable any platform backend, so builds shipped the non-persistent mock backend where writes appear to succeed but live only in memory. Windows Credential Manager and Linux Secret Service (GNOME Keyring / KWallet) are now enabled; one more password entry after upgrading, then reinstalls and upgrades keep it. Headless systems without a Secret Service provider still fall back to prompting

## [2.4.0] - 2026-11-06

This release tightens interaction and appearance: quick connect returns as a dialog, the home title bar keeps a permanent way back to the workbench, the light theme was rebuilt on neutral greys measured from FinalShell with a proportional UI font, and the in-app version now comes from the running binary.

### Added

- Persistent "Back to workbench" button in the home title bar, so returning no longer requires picking a row from the session history
- Dimensional document icon in the file list: the folded corner now reads, and both themes share one set of neutral overlays
- Keychain read/write evidence events (ipc.keychain_read / ipc.keychain_write) recording exit status and secret length - never the secret - to diagnose "password required again after an upgrade"

### Changed

- Quick connect is a dialog again instead of a tab: it no longer occupies a tab slot, and search plus recent-first ordering behave as before
- The sidebar and About card read the version from the running app (Cargo package version plus debug-build flag) instead of the hard-coded 0.1.0, and packaged builds no longer label themselves as a development build
- Light theme backgrounds, panels, borders and text moved to neutral greys, dropping the blue radial wash behind the body; UI text switched to a proportional font (the terminal keeps its own palette and monospace font, size still 13px)

## [2.3.0] - 2026-11-06

This release centres on the monitor pane: processes now report their docker container and come with a sortable popup, while metric collection became incremental so the pane no longer shows bogus 0% / N/A frames.

### Added

- Process → container attribution: the process table resolves each PID's docker container name via cgroup; non-container processes stay blank
- Process popup: sortable by CPU / memory / image name, with one-click copy for PID and container name; the full process list is streamed only while the popup is open and falls back to the Top list once closed
- Per-host test-connection timeout: upper bound for the "Test connection" button (default 30s, 0 = unlimited)
- The "+" in the tab bar now opens a Quick Connect tab, and a new home button returns to the host list
- File list column widths are drag-resizable and align exactly with the rows

### Changed

- Lighter metric collection: docker container names are queried only on first run or when an unknown container appears; the cgroup scan is incremental per round with a full scan every 40 rounds
- Bad-frame protection: frames that timed out mid-output or lack CPU/memory sections are dropped, so the pane keeps the last valid frame

### Fixed

- Disk table in the narrow left pane: available/total values overlapped the usage column and digits were hidden behind the progress bar; process names missing at small widths
- File list column-width dragging (misaligned after a drag, unable to snap back)

## [2.2.0] - 2026-11-05

### Added

- Per-host SSH connect timeout (covers TCP connect + handshake + auth; default 30s, 0 = unlimited)
- Preset command templates: "内存" / "CPU" docker stats read-only queries
- Disk table paths can be drag-panned horizontally, show the full path on hover, and copy on click

### Changed

- Files pane: light theme white canvas / grey header strip / zebra rows / light-blue selection / 3D golden folder icons (FinalShell look)
- Left monitor pane: green CPU and peach memory/swap bars, blue process header with blue-grey zebra, orange/green network curves, mint available-space disk bars
- Window tab bar: light-grey strip with white rounded tab pills and a blue active underline

## [2.1.1] - 2026-11-05

### Fixed

- SFTP upload/download no longer fails to build on Windows: the local file mtime and permissions are now resolved in a cross-platform way (MetadataExt on Unix, SystemTime on Windows), and Windows no longer writes back POSIX permission bits

## [2.1.0] - 2026-11-05

This release completes the remote host workbench: the Files (SFTP) pane, server monitoring and the host list are fully built out, the terminal look (palette / fonts / background image) is fully customizable, and a global command palette with user-defined command templates is added.

### Added

#### Files (SFTP)

- Dedicated "Files" tab with a path bar and a centered toolbar; the remote directory tree loads lazily and stays in sync with the file list; the list gains a type column and splits owner / group into two columns
- Drag-and-drop upload (including recursive folder upload) and recursive directory download
- Permission dialog: three scopes × read / write / execute checkboxes with a live octal and rwx preview
- Open remote files in an external editor and write changes back on save, with batch editing; editors can be picked from the system or specified by path
- Transfer ledger and a floating transfer-records popover: progress, speed, pause / resume, retry on failure and "open directory"
- Configurable transfer behaviour: chunk size, resume check and post-completion hash verification, global / per-host concurrency, global and per-task rate limits, retry count and interval, session idle timeout, overwrite policy, metadata preservation, file blacklist and hidden-file display

#### Terminal appearance

- Terminal palettes expanded to multiple schemes × light / dark (follow the UI / always dark / always light), default deep blue
- Background images from built-in wallpapers or local files, with opacity, blur and an average-colour wash underneath
- Bundled monospace fonts DejaVu Sans Mono / Liberation Mono / JetBrains Mono (300–700) with their licences, CJK glyphs resolved through an explicit fallback chain; defaults are 14px, line height 1.25 and a block cursor

#### Productivity

- Global command palette (⌘K): built-in commands plus user-defined "command templates" maintained in Settings → Command Templates; selecting one writes it into the active terminal and executes it, with multi-line commands submitted as a bracketed paste
- Shortcuts settings page: click any binding to re-record, Esc cancels, remapping takes effect immediately
- New blank terminal window: double-clicking a host connects into that window

#### Connectivity and integration

- External bastion connections: parse external links to open a new local session, support single-channel / SFTP-only sessions, keep passwords in memory only
- SSH key management now generates real keys via ssh-keygen (ED25519 / RSA / ECDSA) with fingerprints and metadata stored
- Host metrics collection and monitoring pane: CPU / memory / network / disk / processes / GPU, collection interval configurable (2s by default)
- Tunnels disconnect automatically on idle timeout
- Session log entry point added; demo mock host data removed

### Changed

- Layout swapped left / right: the monitoring pane moves to the left rail and the host list joins the right-hand tabs; the legacy Inspector and sidebar components are removed
- The connection panel (MOTD) no longer shows this application's branding and keeps only server status
- Echo of injected commands is stripped locally by content, removing the duplicated echo in prompts
- On narrow windows (≤1180px) the right rail becomes an overlay so the host list stays reachable

### Fixed

- Terminal background image not visible: class name clashed with the xterm root element, and the instance backdrop covered the image layer
- The Save button stayed disabled after changing a background image or toggling an inline switch: dirty detection missed settings without their own row
- Font preview now uses the terminal background colour and correctly overlays the background image
- The title bar notification button no longer shows a fake toast; real notifications are used instead

## [2.0.0] - 2026-10-15

This release is centered on **a full AI assistant**: Rhost evolves from a pure SSH terminal manager into a remote host workbench with a built-in AI assistant, supporting conversational terminal operations, Plan-and-Act task planning and multimodal input.

### Added

#### AI assistant

- Complete AI assistant: streaming chat, Markdown and code rendering, code blocks with line numbers plus insert-to-terminal / execute buttons
- Message actions: copy and regenerate for assistant messages; edit-and-resend, copy and delete for user messages
- Reasoning visualization: the thinking process is collapsed independently with a "Thinking…" pulse animation and can be expanded / collapsed manually
- Multimodal input: add image and text file attachments by picker, paste or drag-and-drop
- Chat outline navigation: jump quickly within long conversations
- Context injection: AI skills, rules and cross-session memory injected into the system prompt, with per-file toggles, injection budget control and settings-panel management
- Terminal toolset: read-only tools (read file, list directory, system info) run automatically without approval; file edits and command proposals require user approval, with automatic backup before edits and rejection of critical system paths; built-in high-risk command detection
- Command output feedback toggle (off by default to keep the zero-feedback privacy boundary); when enabled, the AI can perceive execution results and act autonomously over multiple steps
- Plan-and-Act task planning: the model drafts a plan for complex tasks, executes it step by step after user confirmation with live per-step status; failed steps can be retried, skipped, aborted or re-planned; a plan converges early once the goal is met or the remaining steps become infeasible
- File upload / download tools: the AI can perform SFTP transfers directly, with a 64MB size pre-check and a clear error guiding users to the SFTP pane when exceeded

#### Workbench and UI

- Right panel: a combined layout for recent sessions, server monitoring and the AI assistant, with a collapsible icon rail kept in sync between the home page and the workbench
- Recent sessions and quick connect: successful connections are recorded automatically and can be reconnected with one click
- Cross-platform title bar adaptation: native traffic lights and custom-drawn buttons are switchable

### Changed

- AI module settings (including API keys) are stripped from configuration import / export
- Release builds disable the WebView native context menu and default shortcuts to avoid overlapping the custom menu; debug builds keep the debugging shortcuts
- Terminal command execution no longer discards output on timeout — partial stdout / stderr already received is preserved
- The API key input supports show / hide toggling
- Large-scale documentation site and README build-out: guides, design documents and architecture decision records (ADRs)

### Fixed

- Residual `/tmp/.ri-*` scripts from AI terminal execution are cleaned up when a session disconnects
- The tab bar enables drag scrolling and the grab cursor only when tabs actually overflow

## [1.0.0] - 2026-10-08

First public release: a cross-platform SSH remote host manager built with Tauri + Vue 3.

### Added

- Core SSH terminal: multi-tab session management with terminal font and cursor settings applied live
- SSH port forwarding: local, remote and dynamic modes
- SSH automatic reconnect on disconnect plus a startup animation
- SSH host key verification (TOFU) with a fingerprint confirmation dialog
- Tab bar context menu and tunnel configuration
- Host groups, key management and home view switching
- Configuration import / export (mandatory PBKDF2-SHA256 + AES-256-GCM encryption)
- SFTP transfer queue: collapsed progress summary and state persisted across restarts
- Custom MOTD ASCII logo
- A complete application logging system
- Settings dialog and the full set of application icons

### Fixed

- Fixed a remote prompt missing the username

[2.0.0]: https://github.com/fuuhoo/rhost/compare/v1.0.0...v2.0.0
[1.0.0]: https://github.com/fuuhoo/rhost/releases/tag/v1.0.0

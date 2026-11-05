# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/).

[中文](./CHANGELOG.zh-CN.md) · **English**

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

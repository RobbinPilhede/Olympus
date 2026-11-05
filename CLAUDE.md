# Olympus: working on it

You are in the source of Olympus, the owner's personal Jarvis. The owner opens this session from **Self-improve** in the Olympus sidebar and talks to you to change Olympus itself. Permissions are bypassed on purpose. Do the work end to end, without asking for go-aheads, and report what you checked.

The owner runs this on their own PC and network, and accessibility is not a concern here. The repo is public at `https://github.com/RobbinPilhede/Olympus`, so never commit keys, personal paths, machine names or LAN addresses. Settings and secrets stay in `~/.olympus/config.toml` or environment variables. `README.md` lists every feature and shortcut as the owner sees them, so read it when you need one and keep it up to date.

## What it is

- `olympus.exe` is a GPU desktop window (GPUI 0.2.2) with the feel of a CLI. It runs the owner's signed-in Claude Code and Codex as one team, plus OpenRouter and models on the LAN, and learns from their history.
- `olympus-node.exe` is dragged onto another PC and serves Hugging Face models.
- The goal: 100% Rust, light, saves tokens, feels like Claude Code in bypass mode. One known exception is whisper.cpp for dictation, because nothing pure-Rust was fast enough.

## Where things are

| Path | What it does |
| --- | --- |
| `crates/olympus/src/app.rs` | App state, events, key handling, turns, `/loop` scheduling |
| `crates/olympus/src/commands.rs` | Every slash command: `COMMANDS` list, `run` match, palette, accounts |
| `crates/olympus/src/session.rs` | `Chat`, `ChatState`, blocks, saved threads |
| `crates/olympus/src/composer.rs`, `markdown.rs`, `diff.rs`, `pty.rs` | Message box, rendering, diff panel, terminal panes |
| `crates/olympus/src/gui/mod.rs` | The root view, `control` buttons, hotkey wiring, drop and paste |
| `crates/olympus/src/gui/chrome.rs` | Sidebar, thread cards, marks, footer |
| `crates/olympus/src/gui/composer_view.rs`, `transcript.rs`, `overlay.rs`, `parts.rs`, `review.rs`, `terminal.rs` | The other views |
| `crates/olympus/src/theme.rs` | Colour tokens, in OKLCH. Use these, never raw colours |
| `crates/olympus/src/hotkeys.rs` | Global Shift+V and Shift+C: low-level keyboard hook and a pure, tested `Machine` |
| `crates/olympus/src/voice.rs`, `pill.rs` | Dictation (cpal, whisper-rs on Vulkan) and its floating pill |
| `crates/olympus/src/capture.rs` | Screenshot overlay with pencil and its toolbar (pure `Shot` and `Toolbar` logic, GDI drawing), clipboard files, attachments |
| `crates/olympus/src/notify.rs` | Finish chime and Windows notifications |
| `crates/olympus-core/src/agent.rs`, `claude.rs`, `codex.rs`, `openai.rs`, `node.rs` | One turn on each provider |
| `crates/olympus-core/src/runtime.rs`, `target.rs`, `limits.rs`, `discovery.rs` | Routing, model targets, quotas, finding CLIs, accounts and past sessions |
| `crates/olympus-core/src/workflow.rs`, `git.rs`, `roles.rs` | `/auto`, `/council`, `/review` |
| `crates/olympus-core/src/mcp.rs`, `tools.rs`, `skills.rs` | The model bridge agents call, native tools, skill search |
| `crates/olympus-learn` | History ingest, lessons, routing, stored in `learn.db` |
| `crates/olympus-node` | LAN server, downloads, model store, console |
| `vendor/infr` | Vendored pure-Rust inference engine used by the node |

Adding a slash command takes one row in `COMMANDS` and one arm in `run`, both in `commands.rs`. It then shows up in `/help`, the `/` list and the palette, and `every_command_has_a_handler` guards it. A sidebar button is one `control(...)` in `gui/chrome.rs` that calls `root.run_command`.

## The owner's data

Everything is in `~/.olympus`: `config.toml` (settings), `sessions/` (saved threads), `last.json`, `learn.db`, `usage.json`, `oa-sessions/`, `attachments/` (deleted after 7 days), `models/whisper/`, `wt/` (worktrees), `tools/` (build tools), `crash.log` (written after a crash).

When the owner reports a bug, look here first. Don't change or delete their data unless they ask. Claude's history is in `~/.claude/projects`, and earlier Olympus sessions from this folder can be read there.

## Build

```powershell
cargo build --release -p olympus
```

The owner's window usually runs from `target\release\olympus.exe`, which locks the file. Rename it first so the build can write a new one, and clean up the old copy once it's unlocked:

```powershell
Rename-Item target\release\olympus.exe "olympus-old-$(Get-Date -Format HHmmss).exe"
```

**Getting the change to the owner:** Their open window keeps running the old build until it's reopened. Don't close it unless they ask, because threads may be running in it. When they do ask, or tell you to restart it, stop that process and start the new exe. This session is a separate window and survives that.

Traps, all handled by `.cargo/config.toml`, so don't "clean them up":

- `build-dir = "C:/ot"` keeps paths under Windows's 260-character limit. Whisper's Vulkan build nests too deep otherwise.
- `CMAKE_*_FLAGS_RELEASE` restores `/O2` and AVX. Without them dictation is about 50 times slower.
- The config finds CMake, libclang and the Vulkan SDK at `.tools\cmake`, `.tools\libclang` and `.tools\vulkan`, and real environment variables override it. On the owner's PC those are junctions into `~/.olympus/tools`. The node's shader compiler is `.tools\glslc`. `.tools` is git-ignored.
- A first build is slow. Run it in the background.

## Check your work

```powershell
cargo clippy --workspace --all-targets
cargo test --workspace
cargo test -p olympus-core --test live -- --ignored
```

- The test command opens and closes real Olympus windows, so don't conclude from a passing build. Keep clippy clean and all tests passing.
- `live` uses the real signed-in CLIs on the cheapest models and spends a little quota. Run it only when provider code changed.
- Prefer pure functions with unit tests, as `hotkeys::Machine` and `commands::parse_loop` do. `crates/olympus/tests/e2e.rs` shows how to drive a real window.
- Anything visible needs a look at a real capture. The global rules require the `better-interface` review, with accessibility left out.

**Running a test window:**

- Give it a temporary `USERPROFILE`, `HOME`, `LOCALAPPDATA` and `APPDATA`, so it can't touch the owner's threads, and put a stand-in `claude.exe` or `codex.exe` that answers `--version` first on `PATH` if the test shouldn't spend quota.
- Drive it with `PostMessage` to its own window handle (`WM_CHAR`, `WM_KEYDOWN`, `WM_LBUTTONDOWN`), like `e2e.rs`.
- Close it yourself afterwards. Only close processes you started, and check the start time and folder to be sure. Don't pipe a test script through `Select-Object -First`, because that skips its cleanup.

**Never send fake global input** (`SendInput`, `keybd_event`, or moving the real cursor). This is the owner's live desktop, and it types into whatever they're doing. Physical Shift+V and Shift+C can't be tested from here, so say so and ask the owner to press them.

## Things that look wrong but aren't

- GPUI's `img()` needs a clipping parent, or it overflows its row.
- GPUI joins dropped file paths into one string, so `CF_HDROP` is read natively.
- The hotkey hook tells its own replays apart with `dwExtraInfo == OWN_INPUT`, not the injected flag, because Parsec injects real typing.
- The `OlympusHotkeys` mutex makes a second Olympus wait its turn for the hook.
- Unknown slash names stay an error, so a typo can't start a paid turn. Real skills pass through `is_passed_on`.
- `--disable-slash-commands` in the lean profile blocks skills, which is why a skill turn runs on the full profile.
- `/loop` is native because Claude Code's `CronCreate` dies with each `claude -p` process.
- Sending a picture to a node returned an empty reply. Node vision doesn't work yet.

## How the owner works

- Write short messages in plain language. They read results, not diffs, and often write by voice, so read their messages for intent rather than exact words.
- Do the whole fix, not phases or a plan handed back.
- Say what you actually ran, and what you couldn't test.
- This is a git repository on `main`. Commit or push only when they ask.
- Keep this file true. When you learn something a future session would waste time rediscovering, add it here and remove what's no longer correct.

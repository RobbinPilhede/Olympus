<div align="center">

<img src="crates/olympus/assets/icon/olympus.svg" alt="" width="88">

# Olympus

**Every model you're signed into, working as one team.**

A fast, keyboard-first desktop app for Windows, written in Rust. It runs your signed-in Claude Code and Codex side by side, adds OpenRouter and models on your own network, and learns from your history.

[![Release](https://img.shields.io/github/v/release/RobbinPilhede/Olympus?color=white&labelColor=black)](https://github.com/RobbinPilhede/Olympus/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-white?labelColor=black)](LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows%20x64-white?labelColor=black)](#get-started)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-white?labelColor=black)](https://www.rust-lang.org)

[Get started](#get-started) · [What it does](#what-it-does) · [Commands](#commands) · [Safety](#safety-and-secrets) · [Build from source](#build-from-source)

<br>

<img src="docs/images/welcome.png" alt="The Olympus window: a thread list on the left, each thread showing its account and model, and the welcome screen with suggested commands" width="860">

</div>

## Why Olympus

If you pay for more than one AI coding tool, you end up juggling terminals, accounts and quotas. Olympus puts them in one window. Your subscriptions stay as they are, because it drives the real `claude` and `codex` programs you're already signed in to.

- **One window, every model.** Claude, ChatGPT, OpenRouter and your own GPUs, each thread on the account you choose.
- **They work together.** Ask a second opinion mid-chat, have several models debate a decision, or let one build while another reviews.
- **It feels like a CLI.** GPU-drawn with [GPUI](https://gpui.rs), driven from the keyboard, and everything is one `Alt+K` away.
- **It spends fewer tokens.** Lean startup, on-demand skills and cheapest-first routing.
- **It learns from you.** Your past corrections become lessons and finished tasks can become skills, and nothing is used until you approve it.
- **It reaches your phone.** Flip one switch and the whole app opens in your phone's browser on the same network. Or text your own Telegram bot from anywhere.
- **It knows what's going on.** Obiview, the pinned Jarvis thread, reads what's on your screen with no accounts or API keys, and files it into a Markdown vault with a graph of how your notes connect.
- **Your chats follow you.** Connect any cloud storage and carry on with the same chat, memory included, on another PC.

## Get started

**You need:** Windows 10 or 11 (x64) with an AVX2 CPU, plus at least one of [Claude Code](https://www.anthropic.com/claude-code) or [Codex](https://github.com/openai/codex), signed in.

1. Download [`OlympusSetup.exe`](https://github.com/RobbinPilhede/Olympus/releases/latest/download/OlympusSetup.exe) and run it. It takes one click, needs no admin rights, and adds Olympus to your Start menu and Desktop. The builds aren't code-signed yet, so Windows may show a SmartScreen warning.
2. Press **+ New**, pick an account, pick a folder, and start typing.

**Updates are automatic.** Each time you open Olympus, it plays a short animation while it checks for a new version. If there is one, it downloads it, verifies its checksum, swaps it in and reopens, and you can skip it while it downloads. If you're offline, it just opens. Anything already running keeps working on the old copy until you close it.

**To uninstall,** use Windows's Installed apps. Your threads and settings stay in `~/.olympus`, so delete that folder too if you want them gone.

> [!WARNING]
> Chats run with permissions **bypassed** by default, so agents can edit files and run commands without asking. Read [Safety and secrets](#safety-and-secrets) first.

## What it does

### Pick the account and the folder for each thread

**+ New** asks which account to use, then where to work: your user folder, any folder you browse to, or a recent one.

<p>
  <img src="docs/images/accounts.png" alt="Account picker listing two Claude accounts and a ChatGPT account, with options to sign in to more" width="49%">
  <img src="docs/images/folders.png" alt="Folder picker listing the user folder, a choose-a-folder option, and recent folders" width="49%">
</p>

### Make the models work together

| Type this | What happens |
| --- | --- |
| `take help from ChatGPT on this` | The agent calls the other model itself |
| `@codex is this safe?` | Asks another model read-only, without leaving this chat |
| `/council which queue fits?` | Several models answer independently, then you get one recommendation |
| `/auto add rate limiting` | Plans, builds in parallel worktrees, runs checks, and gets a review from the other vendor |
| `/review` | The other vendor reviews your uncommitted changes |

```mermaid
flowchart LR
    You([You]) --> O[Olympus]
    O --> C[Claude Code]
    O --> X[Codex]
    O --> R[OpenRouter]
    O --> N[olympus-node on your LAN]
    C <-->|model bridge| X
    O <--> L[(learn.db: lessons and routing)]
```

### Obiview, your Jarvis

Obiview is the thread pinned at the top of the list, with the Jarvis mark. It has three parts:

- **A vault of plain Markdown** in `~/.olympus/vault`, which you can also open in Obsidian. It has `[[links]]`, `#tags`, properties, backlinks, unlinked mentions, daily notes, search with `tag:`, `file:`, `path:`, `[property]`, `-not` and `"phrases"`, and a graph you can pan, zoom and drag. `Alt+K` finds any note.
- **A reader.** Flip the switch on the thread and it reads the text of the apps on your screen through Windows UI Automation, like Teams, Outlook, your browser, editors and terminals, and notes which programs start and stop. It needs no accounts, passwords or API keys, and it writes what came in and what you did into each day's note, with a note per app in `Sources/`. It only sees what those apps show, so mail you never open isn't read. While you're away from the PC it looks every 5 minutes instead of every few seconds.
- **Talk to Jarvis.** Hold **Shift+B** and speak, or press it twice to keep listening until you press it again. Jarvis answers out loud in Windows' own voice, writes notes when you ask, and does things on your computer. A task from your own words starts straight away. A task based on something it read, like an email, is said out loud first and waits for your yes, so a planted instruction can't run by itself. `/talk <words>` does the same by typing.
- **Jarvis in text.** Ask in the thread, or from any thread with `/jarvis what came in this morning?`. Models in other threads can also look things up themselves when you ask them what's going on.

Each day's note says how long you spent in each program and what you had open, which files you saved and which app was in front at the time, the Git commits in those folders, and new text that appeared on screen. Saved files are noticed in your home folder, leaving out hidden folders, `AppData` and build output. It's all written on your PC.

It skips password fields, password managers, private browsing, banks and windows that ask not to be captured, and files saved while one of those is in front. It only reads what apps show. A minimized browser, Discord or Teams window stops updating until you open it again, so new messages in one may not be noticed until then.

**Choose what it reads on the Apps tab** (`/obiview apps`). It lists your installed and open apps with their icons. **Every app starts off**, and each can be set to **Read**, **Time only** (how long and the window titles, no text) or **Off**. To read everything but a few, choose "Every app except those I turn off". Sites inside your browser have their own list, and you can turn off text, windows behind the front one, saved files and program tracking separately. `/obiview exclude <word>` skips any window with that word in its title, and `/obiview include <word>` undoes it. Raw text is deleted after 14 days. Daily notes and summaries stay.

Jarvis tidies your notes by itself. Once you've chosen an app, Haiku rewrites today's summary every 15 minutes and files lasting facts under `Knowledge/`. It adds to your notes and never rewrites what you wrote. `/obiview summaries off` stops it. Summaries and Jarvis run on your connected Claude or ChatGPT subscription and never use API credits or OpenRouter. Mail, chats and meetings stay out of those unless you add `messages`, as in `/obiview summaries haiku messages`. Asking Jarvis something still sends it what was read, including messages.

### Open it on your phone

Flip **Phone access** at the bottom left, then scan the code it shows. Olympus opens in your phone's browser, drawn from the same state as the window: your threads, live replies, commands, model pickers, changes, terminals, Obiview and the graph. Add it to your home screen to use it like an app. It works in any browser on the same network, and the switch stays how you left it across restarts.

Your phone stays paired, so next time just open the address. `/mirror forget` unpairs everything. Windows asks once for permission to let your network in. To dictate, use your phone keyboard's microphone.

### Reach it from anywhere

Text your own Telegram bot or send it a voice memo, and Olympus replies when the work is done. Olympus has to be open on your PC. It only makes outgoing connections, so nothing on your PC is exposed to the internet, and voice memos are transcribed on your own GPU.

1. In Telegram, message [@BotFather](https://t.me/BotFather), send `/newbot`, and copy the token it gives you.
2. In Olympus, run `/telegram connect <token>`.
3. Send your bot the code Olympus shows. Only that chat can reach Olympus from then on. Everyone else gets silence, and pairing closes after five wrong codes.

In Telegram, `/threads` lists your threads, `/use 2` switches to one, `/new` starts one, `/stop` stops the current work, and `/schedules` lists your schedules. Agents still run with permissions bypassed, so treat your Telegram account as a key to your PC.

### Carry on from another PC

Connect any storage, and your open threads stay the same on every PC, including the memory the model needs to continue. Pick up a chat on another PC and your next message carries on where you left off.

1. Run `/sync` and pick where your chats go. OneDrive, Dropbox, Google Drive and iCloud are offered if their apps are installed, and any other folder works, including a network share or USB drive.
2. Olympus shows a code. On your other PC, run `/sync connect <the same folder> <code>`.

To use a service with no desktop app, such as S3, Cloudflare R2, WebDAV, or Google Drive without its app, install [rclone](https://rclone.org), set it up with `rclone config`, and run `/sync connect <remote>:<folder>`.

- **Live, with a short delay.** Olympus checks every 5 seconds for a folder and every 30 for an rclone service, and a cloud app can add its own delay. You can watch a chat stream from the other PC, and it won't take a new message there until that turn finishes.
- **Encrypted before it leaves.** Your storage provider only sees encrypted files, and the key comes from your code. If you lose the code, run `/sync code` on any PC that's still connected.
- **Nothing is overwritten.** If you change the same chat on two PCs at once, you keep both, and the second gets its own thread marked with the PC it came from.
- **Only your threads travel.** Old history that isn't open in your sidebar stays where it is. Settings, sign-ins, schedules, project files and attachments don't sync, so sign in on each PC and keep your code in step yourself, with Git for example. A project found under the same path in your user folder is used automatically. Otherwise the chat tells you it's working in your user folder.
- **Transcripts over 256 MB stay behind.** The other PC then continues from a summary of the recent conversation.

### Run things on a timetable

`/schedule daily 08:00 summarize my calendar` or `/schedule every 1h check the deploy` keeps running after a restart, and sends the result to your phone if Telegram is connected. Schedules run while Olympus is open. A run missed while it's closed happens once at the next launch. Each run starts a fresh conversation in that thread, so a daily job doesn't pay for its earlier runs.

### Find anything you've talked about

`/search rate limiting` searches everything said in your past Claude and Codex chats, and picking a result reopens that session. Agents can search it too, through the `search_history` tool. Chats on OpenRouter and network models aren't indexed yet.

### Keep what worked as a skill

After a task that took eight or more steps, Olympus offers to save how it was done. `/save-skill` has a model write a draft and shows it to you. Nothing is used until you approve it with `/keep-skill <name>`, which installs it to `~/.claude/skills` and never replaces a skill you already have. `/drop-skill <name>` deletes a draft.

### Run models on your own hardware

Drag `olympus-node.exe` onto another PC on your network and run it. It serves models from Hugging Face, and Olympus finds it automatically. Use `/nodes` to find one, `/hf qwen3` to download a model, and `/load` to load one you already have.

### Talk and point, from any app

Hold **Shift+V** to dictate into whatever app you're using. It runs on your GPU, works offline after the speech model's first download, and understands Swedish and English. Hold **Shift+C** to screenshot a region, draw on it, and drop it into your message.

### Save tokens

- **Lean startup.** Chats skip unused MCP servers, skills and hooks: about 37k startup tokens drop to about 12k on Claude. `/profile full` restores your whole setup for a chat.
- **Skills on demand.** Agents search your installed skills and load only the one they need.
- **Tiers and routing.** `/auto` tasks and `/model auto` chats start on the cheapest tier that fits. Failed checks escalate to a stronger model.
- **Failover.** When a vendor hits its limit, work continues on the other with a short handoff.
- **Bounded memory.** At most 400 tokens of lessons per session, and 20% of runs get none so the benefit can be measured.

### Learn from your history

- History from `~/.claude` and `~/.codex`, including compressed archives, is read into `~/.olympus/learn.db` in the background. It stores signals, short prompts, and your messages and the replies for search (up to 4,000 characters each), with anything that looks like a key removed. Tool output isn't stored. The file stays on your PC.
- `/learn distill` asks a model to find lasting rules in your corrections. Nothing is used until you `/approve` it.
- Lessons that lead to failures or corrections lose standing and retire on their own. Lessons tied to a file go stale when that file changes.
- `/auto` tasks count as successes only if their checks pass. Chat turns count against a model if your next message corrects it.

## Commands

Everything is reachable from `Alt+K`.

<details open>
<summary><strong>Chats and models</strong></summary>

| Type this | What happens |
| --- | --- |
| Any message | Goes to this chat's model, with permissions bypassed |
| `/model auto` | Routes each new chat by task type and learned results |
| `/account`, `/account add work`, `/account add chatgpt work`, or click a thread's account line | Chooses that thread's Claude or ChatGPT account, or signs in to another. Picking an account on the other provider moves the thread there. Chats and `@` questions use it; `/auto`, `/council` and `/review` use your default accounts |
| **+ New** | Asks which account to start the new chat on, then which folder. `Alt+N` and `/new` start right away on this thread's model, account and folder |
| The thread list | Each thread shows its topic (from your first message, or `/rename`), who it's connected to, and that account's usage. Drag a thread up or down to move it; the order is kept after a restart |
| `/sessions` | Reopens any past Claude or Codex session |
| `/simplify`, `/code-review`, `/my-skill ...` | Runs any installed skill or Claude Code command. On Claude it's the real command, with your full setup for that message. On ChatGPT, nodes and OpenRouter, skills are sent as instructions |
| `/loop 5m check the deploy`, `/loop run tests every 1h`, `/loop stop` | Repeats a message or skill in this thread while Olympus is open. Without a time, the model decides how long to wait. A failed run or `/stop` ends it |
| `/schedule daily 08:00 <message>`, `/schedule every 1h <message>`, `/schedule list`, `/schedule stop 1` | Runs a message or skill on a timetable that survives restarts. It reports in this thread and on your phone |
| `/search <words>` | Finds words in your past Claude and Codex chats, and reopens the session you pick |
| `/save-skill [name]`, `/keep-skill <name>`, `/drop-skill <name>` | Drafts a skill from this chat, then approves or deletes the draft |
| `/mirror`, `/mirror on`, `/mirror off`, `/mirror forget` | Shows how to pair a phone, turns phone access on or off, or unpairs every device |
| `/obiview`, `/obiview on`, `off`, `pause`, `resume`, `now` | Opens Obiview, controls its reading, or writes today's note now |
| `/obiview summaries <model>`, `/obiview summaries off`, `/obiview exclude <app>` | Chooses who writes summaries, which sends them what was read, or stops Obiview reading an app |
| `/note <name>` | Opens a note in your vault, or starts a new one |
| `/jarvis <question>` | Asks what's going on, from what Obiview has read and your notes |
| `/telegram connect <token>`, `/telegram`, `/telegram off` | Connects your own Telegram bot, shows its status, or disconnects and deletes its token |
| `/remember always use pnpm` | Teaches Olympus directly |
| `/usage` | Quotas, spend, and whether memory is actually helping |

</details>

<details>
<summary><strong>Code and windows</strong></summary>

| Type this | What happens |
| --- | --- |
| `/merge` | Brings the finished `/auto` branch into your branch |
| `/changes` | Shows this chat's diff beside the conversation |
| `/grid` | Shows up to four windows at once |
| `/claude`, `/codex`, `/term` | The real programs, in a window |
| The wrench at the bottom of the sidebar, or `/self-improve` | Opens Claude Code in its own window, on Olympus's source, permissions bypassed. `CLAUDE.md` briefs it, and it stays open when Olympus restarts. It needs a build made from a clone of this repo |

</details>

<details>
<summary><strong>Local models</strong></summary>

| Type this | What happens |
| --- | --- |
| `/nodes`, `/hf qwen3`, `/load` | Finds a node, downloads a model and loads it if the node is idle, or loads one you already have |
| `/pf qwen3` | Same as `/hf`, from Pirate Face's listings, a third-party site. It uses the torrent's direct link first, and the torrent network only if that fails, then stops sharing. Only download what you have the right to use |
| `/learn distill` | Turns your past corrections into lessons for you to `/approve` |

</details>

<details>
<summary><strong>Voice, screenshots and files</strong></summary>

| Type this | What happens |
| --- | --- |
| Hold **Shift+V**, or press it twice | Dictate in any app: the text is typed where your cursor is when you stop. Pressing it twice keeps listening with nothing held, until you press Shift+V again. A single tap still types V |
| Hold **Shift+B**, or press it twice | Talk to Jarvis from any app. Pressing it again while it's talking cuts it off |
| Hold **Shift+C**, or press it twice | Screenshot in any app: drag a region or click for the whole screen, draw on it with the mouse if you like (`Ctrl+Z` undoes a stroke), then press **Send to Olympus** or Enter. It lands in the message box, unsent. Esc or right-click cancels. A single tap still types C |
| Drag files onto the window, or paste them or an image | Adds them to your next message. `/screenshot` does the same as Shift+C, and `/hotkeys off` frees all three keys |
| Pictures in a chat | Pictures a model links to or names by path, and ones you attach, show in the chat. Click one to open it, and click any link to open it |
| `/notify on`, `off`, `test` | A chime when work finishes, plus a Windows notification when you're not on that thread |

</details>

### Keys

| Keys | |
| --- | --- |
| `Alt+K` | Find anything |
| `Alt+1-9`, `Alt+←/→` | Switch windows |
| `Alt+G` | Grid view |
| `Alt+D` | Changes panel |
| `Alt+B` | Show or hide threads |
| `Alt+N`, `Alt+T`, `Alt+W` | New chat, new terminal, close (or the `×` on a thread; `/reopen` brings a closed chat back) |
| `Enter`, `Ctrl+J`, `Esc` | Send, new line, stop |
| Drag, `Ctrl+C` | Select and copy text (`Ctrl+Shift+C` in a terminal) |

## Configuration

Settings live in `~/.olympus/config.toml`, which is created on first run. It holds your model tiers, node addresses, parallel agent limit and quota ceiling.

Keys are never stored in this repo or in the app. Set them in your environment:

| Variable | Used for |
| --- | --- |
| `OPENROUTER_API_KEY` | OpenRouter models (or set `openrouter_key` in `config.toml`) |
| `HF_TOKEN` | Gated Hugging Face models on a node |
| `OLYMPUS_NODE_TOKEN` | The token a node requires. Set the same value as `node_token` in `config.toml` |
| `OLYMPUS_MIRROR_PORT` | The port for phone access, instead of 47480 |
| `OLYMPUS_TELEGRAM_TOKEN` | Your Telegram bot's token, instead of the one `/telegram connect` saves in `~/.olympus/telegram.json` |

Your data stays in `~/.olympus`: settings, saved threads, `learn.db`, usage and attachments.

## Safety and secrets

> [!CAUTION]
> Olympus is built for one person on their own PC and trusted network. Read this before you use it anywhere else.

- **Agents run with permissions bypassed.** They can change or delete files and run commands without asking. Point them at folders you can recover, ideally under version control.
- **There's a small block list, not a sandbox.** Before a chat's shell command runs on Claude, Codex, OpenRouter or a node, Olympus refuses the most catastrophic ones: recursively deleting system, home or credentials folders, wiping or formatting a disk, damaging Windows or its backups, and force-pushing over `main`. It catches accidents, not a model determined to get around it. It doesn't cover `/claude`, `/codex`, `/term` or Self-improve windows, or file edits. If a command you really want is refused, run it yourself in a terminal.
- **Change the node token.** A node accepts the default token `olympus` unless you set `OLYMPUS_NODE_TOKEN`. Its firewall rule only allows your local subnet, but anyone on that network who knows the default can use the node.
- **A paired phone can run commands on your PC.** Phone access refuses addresses outside your own network, needs a one-time code to pair, and only allows your local subnet through Windows Firewall. The connection is plain HTTP, though, so anyone able to watch your network can read it and copy the device's key. Use it on a network you trust. `/mirror forget` unpairs everything if you lose your phone.
- **Obiview reads a lot, including other people's words.** What it reads stays in `~/.olympus/obiview.db` unencrypted, readable by anything running as you. Reading work mail and chats may be against your employer's rules, so check first. When you ask Jarvis or turn on summaries, what it read goes to that model's provider. Mail can also contain text written to trick an AI, so Jarvis and summaries run read-only and Olympus writes their notes itself. Agents in your other threads have full access and can look this text up when you ask them to. They're told to treat it as information only, but that's a warning, not a wall.
- **Your Telegram account can run commands on your PC** once it's linked. Keep your bot token private, and use `/telegram off` if you lose your phone.
- **Your sync code opens all your synced chats,** which often contain secrets from your work. It's saved in `~/.olympus/sync.json`. Anyone with both the code and your storage could read your chats, or add a chat that misleads an agent you then continue it with. If it leaks, run `/sync off`, delete the storage folder, and connect again to get a new code.
- **Global hotkeys.** Shift+V, Shift+B and Shift+C use a low-level keyboard hook. `/hotkeys off` turns it off.
- **Automatic updates.** The installed app downloads new versions from this repo's releases over HTTPS and checks each one's SHA-256. The releases aren't code-signed, so you're trusting this GitHub repo. Builds you make yourself, and an `olympus.exe` run from outside the install folder, never update themselves.
- **Your accounts, your terms.** Each provider's terms apply to how you use your subscriptions and models. Olympus isn't affiliated with Anthropic or OpenAI.

## Build from source

You need the Rust toolchain with MSVC, plus CMake, libclang, the Vulkan SDK and `glslc`. Point Cargo at them in either of these ways:

- **Environment variables:** set `CMAKE`, `LIBCLANG_PATH` and `VULKAN_SDK`.
- **A local `.tools` folder (git-ignored):** use `.tools\cmake`, `.tools\libclang` and `.tools\vulkan`.

The node also needs its shader compiler at `.tools\glslc\Bin\glslc.exe`. Neither program needs any of these at runtime.

```powershell
git clone https://github.com/RobbinPilhede/Olympus.git
cd Olympus
cargo build --release
```

The binaries land in `target\release`, and build files go in `C:\ot` to stay under Windows's path length limit. A first build is slow. `scripts\release.ps1` builds and publishes the version named in `Cargo.toml`.

```powershell
cargo clippy --workspace --all-targets
cargo test --workspace                              # opens and closes real Olympus windows
cargo test -p olympus-core --test live -- --ignored # uses your signed-in CLIs and spends a little quota
```

### Project layout

| Path | Purpose |
| --- | --- |
| `crates/olympus` | The desktop interface |
| `crates/olympus-core` | Providers, workflows, the MCP bridge |
| `crates/olympus-learn` | History ingest, lessons, routing |
| `crates/olympus-node` | The LAN model server |
| `vendor/infr` | The pure-Rust inference engine ([infr](https://github.com/kryptic-sh/infr), MIT), embedded in the node |

## License

[MIT](LICENSE). Vendored code and fonts keep their own licenses: see [`vendor/infr/LICENSE`](vendor/infr/LICENSE) and [`OFL.txt`](crates/olympus/assets/fonts/OFL.txt).

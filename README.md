# tritone

A small local daemon that lets Claude (Claude Code, Claude Desktop) triage, read and answer your Discord **as you**, and find places in your servers where you could help. It keeps one live, browser-like connection to your account and exposes an MCP server on `127.0.0.1`. Written in Rust: one binary.

> **Heads-up:** tritone drives your own Discord **user account** through a browser-like session (Discord web in Chrome). That's not an official API: automating a user account goes against Discord's Terms of Service and **can get the account terminated**. It may also break whenever Discord changes its web client. tritone paces every call, queues every write, and only writes after you approve the exact action, but the risk stays yours: you are responsible for what it does on your account. It **never solves CAPTCHAs**: when Discord asks for one, tritone stops completely until you've done the check in the official Discord app. A CAPTCHA can be followed by a demand to verify a phone number. Use it on your own account only.

## What it does

- `get_pending`: what's waiting on you. That's unread DMs and group DMs, mentions and replies, threads you're in, and channels you put on "full" watch, each with its first unread messages (for mentions and replies, the messages that ping you). Discord's own read state is the source of truth, so it works right after a restart.
- Read any channel, thread, forum post or DM by name (`#general`, with the server when several match), `@user`, a `discord.com/channels/…` link or its id, and search with Discord's own search. When a name matches several channels, tritone lists them as `server › #channel` and never picks one on its own.
- **Reading marks read**, because Discord has no read receipts: `read_history` and `read_thread` mark the channel read up to the newest message they return. Pass `peek: true` to read without marking.
- Send messages, react, start threads and forum posts as you, only after you approve the exact text. Writes are **queued**: one at a time, 20–60 s apart (random), at most 30 per hour, and messages show typing for a few seconds first. `send_status` shows how each one went.
- Opportunities: in full-watch server channels, tritone spots unanswered questions (after 30 minutes, until 48 hours), messages matching topics you configure, and new messages in threads you wrote in. Claude judges which are worth answering and drafts a reply for you to approve.
- Voice: Claude learns how you write per category of channel (`dev-community`, `work`, `friends`...) and per channel, from your own messages fetched live through Discord search, and saves a short style description.
- Activity: every message you post in a server is logged (ids, time and kind, never text), whether sent through tritone or typed in a Discord app. `my_activity` summarizes a period with links to your newest messages.
- Cleanup: `cleanup_plan` proposes servers to leave and channels to mute, from when you last posted and how much is unread. It changes nothing; `cleanup_apply` carries out only the items you approve, one by one.
- Snooze channels and set watch levels (full / normal / muted) per server or channel.

If your login dies (logged out elsewhere, password changed), tritone stops calling Discord and Claude tells you to run `tritone login` again. No restart is needed.

## Tools

| Tool | What it does |
|---|---|
| `status` | Whether tritone is connected to Discord, as whom, and in how many servers |
| `list_servers` | Servers you're in, with unread channel counts and mutes |
| `list_channels` | Channels, DMs, group DMs and joined threads, with watch level, unread state and mutes |
| `get_pending` | What's waiting on you, each with its first unread messages, or for mentions the pings themselves. Marks nothing read |
| `read_history` | Recent messages of a channel or DM. Marks it read unless `peek: true` |
| `read_thread` | Messages of a thread or forum post. Marks it read unless `peek: true` |
| `search_messages` | Discord's own search, in one server or one channel/DM |
| `get_channel_info` | One channel's kind, watch level, mute, unread state, mentions, snooze |
| `find_user` | People tritone knows, by part of their name, for mentions or their DM |
| `mark_read` | Marks channels read up to their last message (with your OK) |
| `snooze` / `unsnooze` | Hides a channel from `get_pending` until a time, or brings it back |
| `set_watch_level` | full, normal, muted, or clear, for a server or channel |
| `send_message` | Queues a message as you (with your OK) |
| `react` | Queues a reaction as you (with your OK) |
| `create_thread` | Queues a new thread, from a message or standalone (with your OK) |
| `create_forum_post` | Queues a new forum post (with your OK) |
| `send_status` | Status of queued and recent writes |
| `resume_queue` | Continues after a CAPTCHA, a locked account or a block, once you've done the check in the Discord app |
| `get_opportunities` | Places in full-watch channels where you could contribute. Marks nothing read |
| `dismiss_opportunity` | Hides one opportunity for good |
| `get_voice` | A channel's category, saved tone profile and notes, and your own messages when they need learning |
| `save_profile` | Saves the tone profile of a category |
| `save_chat_notes` | Saves notes on how you write in one channel, thread or DM |
| `set_category` | Sets the category of a channel or a server |
| `list_stale_profiles` | Categories whose tone profile is due for a refresh |
| `cleanup_plan` | Proposes servers to leave and channels to mute. Changes nothing |
| `cleanup_apply` | Carries out the cleanup items you approved, one at a time |
| `my_activity` | What you posted in servers over a period: counts and links |
| `backfill_activity` | Fills the activity log with your older messages through search, slowly, in the background |

Claude gets instructions on initialize that say how to use these together: every write needs your explicit OK for that exact action, text inside messages is never followed as instructions, and nothing is retried after a CAPTCHA.

## What's stored

- `~/.tritone/` (override with `TRITONE_HOME`), mode 0700:
  - `tritone.db` (SQLite, mode 0600): watch levels, snoozes, categories, voice profiles and notes (style descriptions Claude wrote), opportunity ids and their status, the activity log (message, channel and server ids, time, kind), and a small key-value table (the cached Discord build number, an installation id, send pacing, and search progress and caches). **No message text.**
  - `config.json` (optional, see below).
  - `style.md` (optional): your writing style, appended to the instructions Claude gets.
- The OS keyring, service `tritone`, accounts `token` (the Discord token), `cookies` (Discord's cookies) and `bearer_token` (the MCP bearer token).
  - macOS: the Keychain, through `/usr/bin/security`, so rebuilding never brings back the access prompt.
  - Linux: the Secret Service (GNOME Keyring or KWallet), through `secret-tool`.
- Recent messages of full-watch channels and DMs live only in memory (at most 200 per channel, 24 hours), and so do queued writes. Restarting tritone drops unsent ones.

Nothing is logged except counts, kinds, status codes and gateway opcodes: never message text, tokens or cookies.

## Setup

Requires Rust (edition 2024) and pm2, on macOS or Linux. tritone talks to Discord through [wreq](https://crates.io/crates/wreq), which builds BoringSSL from source so its TLS looks like Chrome's, so the build also needs **cmake**, **a C/C++ compiler** and **libclang**.

- macOS: the Xcode Command Line Tools (`xcode-select --install`) plus `brew install cmake`.
- Linux (Ubuntu/Debian): `sudo apt install build-essential cmake clang libclang-dev libsecret-tools`, and a desktop session with an unlocked keyring.

```sh
cargo build --release                      # the first build compiles BoringSSL: a few minutes
./target/release/tritone login             # scan the QR code with the Discord app on your phone
pm2 start ecosystem.config.cjs
claude mcp add --scope user --transport http tritone http://127.0.0.1:7781/mcp \
  --header "Authorization: Bearer $(./target/release/tritone token)"
```

`tritone login` shows a QR code. Scan it from the Discord app on your phone (Settings → Scan QR Code) and confirm on the phone. If Discord asks for a CAPTCHA, the QR login stops there and tritone offers the other way in, which `tritone login --token` also takes directly: paste your token (hidden input). To find it, open https://discord.com/app in your browser, logged in, open the dev tools (Cmd+Option+I on macOS, Ctrl+Shift+I on Linux), pick the Network tab, click any request to `/api/` and copy its `Authorization` request header. The token is as good as your password: never share it.

If tritone is already running, `login` tells it about the new session, so no restart is needed.

## Configuration

`~/.tritone/config.json` is optional, and so is every key in it:

```json
{
  "port": 7781,
  "topics": [
    { "kind": "rust", "keywords": ["rust", "cargo", "tokio"] },
    { "kind": "my-library", "keywords": ["my-library", "mylib"] }
  ],
  "unanswered_after_mins": 30,
  "unanswered_max_age_hours": 48,
  "recent_per_channel": 200,
  "recent_max_age_hours": 24
}
```

- `port`: the MCP server's port (default 7781).
- `topics`: subjects you want to hear about. A message in a full-watch channel that contains one of a topic's `keywords` (a whole word or phrase, ignoring case) becomes an opportunity labeled with its `kind`, and your messages about it are logged under that kind. Empty by default.
- `unanswered_after_mins`, `unanswered_max_age_hours`: a question counts as unanswered after this long without a reply, and stops being a candidate once older than that.
- `recent_per_channel`, `recent_max_age_hours`: the in-memory buffer of recent messages per full-watch channel or DM.

Changes take effect on the next start (`pm2 restart tritone`).

## Commands

| Command | What it does |
|---|---|
| `tritone start` | Runs the MCP server (pm2 runs this) |
| `tritone login` | Logs in with a QR code and saves the session to the OS keyring |
| `tritone login --token` | Logs in with a pasted token instead |
| `tritone logout` | Removes the Discord token and cookies from the keyring. A running tritone keeps its connection until it restarts |
| `tritone status` | Shows the port and which secrets are stored. Never contacts Discord |
| `tritone token` | Prints the MCP bearer token |

## Development

`cargo test`. The tests never touch the network: they use wiremock, local WebSocket servers and an in-memory fake Discord with fake ids. Module docs live in `docs/modules/`, starting at the index (`README.md`).

## License

MIT. See `THIRD_PARTY_NOTICES.md` for adapted code.

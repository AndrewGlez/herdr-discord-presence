# Herdr Discord Presence

Shows the focused Herdr folder and whether an agent is running in Discord Rich Presence.

## Setup

1. Create a Discord application at <https://discord.com/developers/applications>.
2. Add image assets named `herdr` and `agent` under **Rich Presence → Art Assets**.
3. Build the Rust plugin:

   ```sh
   cargo build --release
   ```

4. Link the plugin (the default application ID is already configured):

   ```sh
   herdr plugin link ~/herdr-discord-presence
   herdr plugin enable local.discord-presence
   ```

The startup hook runs after Herdr restores its session and polls every five seconds.
Discord must be running locally. Set `HERDR_DISCORD_INTERVAL` to change the interval.
Set `HERDR_DISCORD_CLIENT_ID` to override the default application ID.

`res/pi.png` is a local Pi reference asset. Discord RPC cannot display local file paths:
upload this PNG to the Discord application as an asset named `pi`. The Herdr logo asset
should be named `logo` (the plugin uses it when no agent-specific image is available).

To start it visibly in a Herdr pane while testing:

```sh
herdr plugin pane open --plugin local.discord-presence --entrypoint presence --placement split --focus
```

The plugin is single-instance: its startup hook owns Discord presence. A second pane or
startup attempt exits instead of overwriting the active Discord activity.

On Linux and macOS, the running presence process detects a newer
`target/release/herdr-discord-presence` binary on its next heartbeat, then re-execs it.
Run `cargo build --release`; no manual process restart is needed. The elapsed timestamp is retained.

When the focused pane runs `lazygit`, presence shows `Lazygit · Git UI` instead of an
agent state. It includes the current branch, commit count, changed-file count, and upstream
ahead/behind numbers when available. It also detects `opencode` directly from foreground process information,
even if Herdr's agent record has not appeared yet. For active OpenCode sessions, it also
queries the local OpenCode SQLite backend for child sessions updated within the past minute,
showing their count and aggregate token use.

Upload an OpenCode art asset named `opencode` to show its dedicated image.

For a working Pi agent, presence also reads its local session JSONL and shows the latest
model and request context size (for example, `pi · working · gpt-5.6-terra · ctx 72.9k`).
This is local-only.

OMP (`omp`) sessions are detected the same way as OpenCode: from the foreground process
information even before Herdr's agent record appears. Active OMP sessions reuse the same
session JSONL parser as Pi, so working presence includes the latest model and context size
(for example, `omp · working · gpt-5.6-terra · ctx 72.9k`), and ready/idle state stays
distinguishable from a generic terminal. There is no dedicated OMP Discord art asset; the
plugin renders OMP with the existing `pi` asset while keeping the visible label `omp`.

Presence labels use emoji for the agent (`🤖` pi/omp, `🟩` opencode, `✳️` claude, `⚡` codex,
`🐙` copilot, `🛸` grok, …), the status (`⚙️` working, `⚠️` blocked, `💤` ready) and the
focused folder/branch (`📁`, `🌿`).

OMP/Pi subagents are read from the parent session's sibling directory
(`<session-stem>/*.jsonl`) and collapsed into a count, not listed individually —
finished subagents (trailing `session_exit`) are omitted. The line shows the total plus a
per-state breakdown only for states that are present (`⏳` working, `⚠️` blocked, `💤` idle).
For example:
`🤖 omp · ⚙️ working · deepseek-v4.1-flash · ctx 288.0k · 🧩 3 · ⏳3`.

Ready agent status includes the total number of open Herdr panes.

## Development

Run `HERDR_DISCORD_CLIENT_ID=... cargo run --release` to test outside Herdr. Unlink with:

```sh
herdr plugin unlink local.discord-presence
```

# Claude Deck

A terminal viewer for Claude Code sessions, standalone or as a herdr plugin:

- **All sessions**: every running Claude Code session in one list, with its name, project, busy or idle, last activity, calls, failures, and asks still waiting for a permission answer. Sessions waiting for you come first, then busy ones. `enter` opens one, `esc` comes back.
- **One session**, in three views of the same data:
  - **Files**: the scratchpad (and `images/`) newest first, `●` on anything written in the last 30 s, with a text or image preview.
  - **Tasks**: background shells (`tasks/b*.output` named by a call's `backgroundTaskId`, exit code from the last line) and subagents (`a*.output`, a symlink to the subagent's transcript). Claude Code also gives every foreground command a `b*.output` while it runs and deletes it afterwards; those are shown as the running call's live output in Calls, not as tasks.
  - **Calls**: every tool call from the transcript and its subagents: time, exit code (only when it failed), duration, command and output. Background shells link to their `tasks/` output. With the hooks below, also who allowed it.

| View | Shape | Stepping through shell commands |
| --- | --- | --- |
| A tabs | Files / Tasks / Calls tabs, a list over a preview | `enter` opens one full screen; `n`/`p` next and previous, `e` next failure, `esc` back |
| B columns | sources │ list │ detail; the detail follows the cursor | `j`/`k` in the list, `l` into the detail to scroll, `n`/`p` from anywhere |
| C timeline | files, tasks and calls in one stream | `enter` expands in place and the expansion follows `n`/`p`; `o` full screen |

`v` switches views and `?` lists every key. `l`/`→` goes in and `h`/`←` comes out, down to the list of
sessions. `x` shows failures only, `/` filters by text, `y` copies the command or path, `=` formats
JSON / JSONL / XML, `#` toggles line numbers, `u` unfolds long output. Previews are highlighted with
bat's syntaxes and theme (built in). Images are drawn with the Kitty graphics protocol, in Ghostty,
kitty and WezTerm directly and inside herdr panes; tmux is not supported.

After `/clear` Claude Code carries on in a new session; the deck notices within a few seconds and
follows it.

## Install

```sh
brew install bigsan/tap/claude-deck   # macOS and Linux, from the release binaries
cargo install claude-deck              # build it from crates.io
cargo install --path .                 # or from a checkout of this repository
```

The [releases](https://github.com/bigsan/claude-deck/releases) also carry the binaries as
`claude-deck-v<version>-<target>.tar.gz` for arm64 and x86_64 macOS and Linux. The Linux
binaries are statically linked (musl), so they do not depend on the distribution's glibc
version.

```sh
claude-deck              # every running session; the cursor starts on this folder's newest
claude-deck 8e379c28     # one session, by its id or the start of it
claude-deck /tmp/claude-501/<project>/<session id>   # or by its directory
claude-deck | cat        # the list as text
```

### herdr

Install the plugin from GitHub, or let an installed binary link one; both register it as
`bigsan.claude-deck`, so use one or the other (herdr refuses to install over a linked plugin):

```sh
herdr plugin install bigsan/claude-deck   # downloads the release binary for this version
```

The install step (`scripts/fetch-release.sh`, needs `curl`, `tar` and `shasum` or `sha256sum`)
puts the binary in the plugin's own `bin/`; when it cannot, it uses a `claude-deck` already on
`PATH` or in the Homebrew or cargo locations. `herdr plugin update` follows new releases. The
plugin is listed in the [herdr marketplace](https://herdr.dev/plugins/).

```sh
claude-deck setup herdr          # or: write a plugin that runs this binary and link it
claude-deck setup herdr --check  # linked, stale (the binary moved), unlinked, or installed from GitHub
```

That plugin folder is `~/.config/herdr/claude-deck-plugin/`; its manifest runs the binary by
absolute path, so run `setup herdr` again after moving it. When the plugin is installed from
GitHub, `setup herdr` leaves it alone and points to `herdr plugin update`. Either way, bind a key in
`~/.config/herdr/config.toml`, then `herdr server reload-config`:

```toml
[[keys.command]]
key = "ctrl+alt+d"
type = "plugin_action"
command = "bigsan.claude-deck.toggle"
description = "toggle claude deck"
```

Actions: `toggle`, `open-a`, `open-b`, `open-c`, `close`, `sessions` (every session in a split),
`path`, `reveal`. The herdr options (`OPEN_ON`, `DIRECTION`, `RATIO`, …) are settings like the
others below. Opening the deck moves the focus to it (or to the deck already open beside
the agent); `FOCUS=0` keeps it on the agent. The deck does not open by itself unless `OPEN_ON` is set.

## Settings and log

`claude-deck --version` prints the version, which `--help` and the `?` screen also show.
Settings are `KEY=VALUE` lines in one file, the herdr plugin's `config.env` in
`$(herdr plugin config-dir bigsan.claude-deck)` (`~/.config/herdr/plugins/config/bigsan.claude-deck/`),
read standalone too; each can be overridden with a `DECK_<KEY>` environment variable.
`config.example.env` lists every key: `setup herdr` writes it beside its manifest, and a
`herdr plugin install` has it in the plugin's folder (needs `jq`):

```sh
root=$(herdr plugin list --json --plugin bigsan.claude-deck | jq -r '.result.plugins[0].plugin_root')
config=$(herdr plugin config-dir bigsan.claude-deck)
mkdir -p "$config"
cp -n "$root/config.example.env" "$config/config.env"   # -n: keeps a config.env you already have
```

It is also [in this repository](config.example.env).

| Key | Default | |
| --- | --- | --- |
| `LOG_LEVEL` | `warn` | `off`, `error`, `warn`, `info`, `debug` or `trace` |
| `LOG_FILE` | `~/.local/state/claude-deck/claude-deck.log` | rotated to `.log.1` past 5 MB |
| `IMAGE_CACHE` | `8` | images kept in the terminal; a cached image is marked `cached` in the lists |

Each log line names its thread, so `DECK_LOG_LEVEL=info` shows image files being read (and
converted with `sips`) on the `image-load` thread and only the sending on `main`:

```text
… main         INFO  image 2-photo.jpg: cursor settled, reading it on the image-load thread
… image-load   INFO  image 2-photo.jpg: converted with sips in 119.0 ms
… main         INFO  image 2-photo.jpg: sent to the terminal as id 93782593: 635.2K of escape sequences
… main         INFO  image 2-photo.jpg: in the terminal's cache (id 93782593), placed without sending
```

## Permission verdicts (optional)

The transcript records what ran, not who allowed it. The `session-deck-hooks` Claude Code plugin
(`claude-plugin/`, also built into the binary) adds that: async hooks on `PreToolUse`,
`PermissionRequest`, `PermissionDenied`, `PostToolUse` and `PostToolUseFailure` append one line per
event to `gate.jsonl` in the session directory. The deck reads it and shows `auto`, `ask`, `deny`,
`rej` or `wait` per call, and the list of sessions counts the asks still waiting.

```sh
claude-deck setup hooks          # or --check
```

It writes a local marketplace to `~/.local/share/claude-deck/marketplace/` and installs the plugin
with `claude plugin`; already installed from anywhere, it does nothing. The hooks only append to that
file. They never block, rewrite or delay a call, and they store no tool input or output, only ids,
the tool name, the permission mode and a hash of the input. The hash is what pairs a
`PermissionRequest`, which has no `tool_use_id`, with its call.

## Layout

| Path | Holds |
| --- | --- |
| `src/data.rs` | readers for the scratchpad, `tasks/`, transcripts and `gate.jsonl` |
| `src/render.rs`, `src/text.rs` | highlighting, formatting, folding and wrapping by display width |
| `src/ui.rs`, `src/views.rs` | rows, details and the three views |
| `src/overview.rs`, `src/sessions.rs` | the list of running sessions, read in the background |
| `src/app.rs`, `src/kitty.rs` | input, the redraw and rescan loop, images |
| `src/config.rs`, `src/log.rs` | settings and the log file |
| `src/herdr.rs`, `src/setup.rs` | herdr actions and events (adapted from herdr-scratchdock), `setup` |
| `claude-plugin/` | the Claude Code hook plugin that writes `gate.jsonl` |
| `herdr-plugin.toml`, `bin/`, `scripts/fetch-release.sh` | the plugin `herdr plugin install` uses; the manifest is generated from `src/setup.rs` |
| `.github/workflows/release.yml`, `packaging/` | release binaries and the Homebrew formula; see [RELEASING.md](RELEASING.md) |

`cargo test` runs the tests; `claude-deck probe <session dir>` prints the parsed session as JSON
lines, for checking the reader against a real session.

Session resolution, the dock state handling and the image letterboxing are adapted from
[herdr-scratchdock](https://github.com/mvaios/herdr-scratchdock) (MIT, Copyright (c) 2026 Vahram Mirzoyan).

## License

MIT; see [LICENSE](LICENSE), which also carries the herdr-scratchdock notice.

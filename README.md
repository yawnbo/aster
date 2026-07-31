# Aster

Aster is a conservative, history-first shell completion system. It would rather
show nothing than offer a completion it cannot justify.

The project is in early development. The current vertical slice provides a
shared local daemon, SQLite-backed command history, Zsh history import, and
an inline completion menu, ghost text, command discovery, lazy descriptions,
native Zsh candidates, and segment-by-segment acceptance.

## Principles

- Completed command history is the highest-priority source.
- Candidate providers are ordered tiers; lower tiers fill unused menu capacity
  but never outrank higher-confidence results.
- The menu appears while typing and highlights the highest-ranked candidate.
- Ctrl-Space accepts one useful segment from that candidate by default.
- No per-command shell completion scripts are required.
- Every host has one shared daemon and one history database.
- tmux panes and concurrent SSH shells remain lightweight clients.
- Silence is preferable to an uncertain completion.

## Build

```sh
cargo build --release
cargo install --path .
```

Aster currently targets Unix systems because its shared transport is an
owner-private Unix socket.

## Zsh Setup

Add one integration line to `.zshrc`:

```zsh
eval "$(aster init zsh)"
```

This is Aster's only shell integration. You do not need to source completion
scripts for every installed program.

The integration:

- Imports the existing Zsh history at shell startup when the file changed since
  the previous import.
- Records submitted foreground commands and the shell status reported afterward.
- Starts the per-host daemon automatically.
- Delegates normal Tab to its previous native Zsh widget after clearing stale
  Aster display state.
- Captures append-safe candidates from the configured Zsh completion system in
  a forked completion context and blends them into Aster's menu.
- Shows ranked candidates automatically as the command buffer changes.
- Renders a bordered, color-highlighted menu and selected-candidate ghost text
  as part of ZLE's multiline display.
- Uses Ctrl-Space to accept the highlighted candidate and preserves its previous
  binding as the fallback when Aster has no candidate.
- Uses Ctrl-N and Ctrl-K to move through an open menu; outside the menu their
  prior widgets remain active.
- Accepts the selected candidate only with Ctrl-Space by default; Enter remains
  the shell's untouched command-submission binding.
- Labels the tmux pane `aster` only while Zsh is idle, then switches the pane
  title to the foreground command until the prompt returns.

Aster owns ZLE's suggestion display. Do not load a second autosuggestion plugin
alongside it; competing `POSTDISPLAY` highlights can recolor or stale the menu.

Native Zsh capture is asynchronous and best-effort. History remains first,
native matches appear before generic command inventory, and duplicate displays
are removed. Aster only imports matches that are safe literal appends at the end
of the buffer. Quoted replacements, mid-word edits, removable suffixes, and
completion functions that bypass `compadd` remain available through ordinary
Tab, which still delegates to Zsh unchanged.

The same setup works inside tmux and on SSH hosts. Each remote host runs its own
daemon and keeps its own local history; tmux panes on that host share it.

## Conservative Acceptance

Given a history entry:

```text
cd ~/dev/gitrepos/aster
```

and the current buffer:

```text
cd ~/d
```

pressing Ctrl-Space on the automatically highlighted candidate inserts only:

```text
ev/
```

Aster then queries again from the new buffer. The initial boundary scanner is
deliberately small and recognizes path and word delimiters; a shell-aware parser
will replace it before mid-line editing is enabled. Set
`completion.accept = "full"` if full-line acceptance is preferred.

## Commands

```text
aster daemon
aster stop
aster doctor
aster init zsh
aster import-history --file ~/.zsh_history
aster record --command "git status" --cwd "$PWD" --exit-code 0
aster complete --buffer "git st" --cursor 6 --cwd "$PWD"
```

Client commands automatically start the daemon if it is unavailable.

## Configuration

The default configuration is written by `aster init zsh`:

```toml
[completion]
max_candidates = 8
key = "ctrl-space"
accept = "segment"

[history]
ignore_leading_space = true
successful_first = true

[ui]
menu_width = 64
max_visible = 6
prompt_offset = 2
border = "4"
accent = "10"
text = "7"
muted = "8"
ghost = "8"
selected_background = "8"
selected_text = "15"
selected_source = "0"
```

`completion.key` also accepts `shift-tab`, `tab`, and Ctrl-letter names such as
`ctrl-i` or `ctrl-x`. Ctrl-J, Ctrl-K, Ctrl-M, and Ctrl-N are reserved by the
menu integration. The generated binding is refreshed when a new shell starts.
UI colors accept ANSI palette indexes from `0` through `255` or exact `#RRGGBB`
values. ANSI indexes follow the active terminal theme. `prompt_offset` is the
visual width of the final prompt line before editable text. The menu follows the
live letter cursor and clamps before the terminal's final column.

Paths can be overridden for testing or custom deployments:

| Variable | Purpose |
| :------- | :------ |
| `ASTER_CONFIG` | Configuration file |
| `ASTER_STATE_DIR` | SQLite database and daemon log directory |
| `ASTER_SOCKET` | Unix socket path |
| `ASTER_TMUX_SHELL_TITLE` | Pane title used while Zsh is at the prompt |
| `XDG_CONFIG_HOME` | Default config root |
| `XDG_STATE_HOME` | Default state root |

Run `aster doctor` to print the resolved paths and verify daemon connectivity.
Custom state and socket paths must live beneath directories owned by the current
user and inaccessible to group and other users.

## Status

Implemented:

- Shared auto-started daemon.
- Versioned JSON protocol over an owner-private Unix socket.
- SQLite WAL history shared by all shells on a host.
- Change-detected Zsh history import.
- Current-directory, success, and recency-aware history ranking.
- Cached `PATH` command inventory, shell builtins, semantic command icons, and
  authored descriptions for common tools.
- Lazy command-description enrichment from exact man pages and sandboxed
  `--help` output, with executable-fingerprinted persistent caching.
- Asynchronous native Zsh candidate capture for append-safe options,
  subcommands, paths, aliases, and other configured completion sources.
- Prefix-only completion with conservative partial acceptance.
- Cursor-anchored, scrolling ZLE menu with ghost text, descriptions, semantic
  kinds, highlighted matches, position counter, and configurable key hints.
- Debounced asynchronous completion requests that never block character input.
- Zsh native-completion fallback.

Description discovery never runs on the completion request path. An unresolved
visible command keeps its origin fallback, is queued on a bounded worker pool,
and is refreshed in the menu when metadata becomes ready. Aster checks the exact
man page first. On macOS, it may then run `--help` through `sandbox-exec` with
network and writes denied, an empty environment, bounded output, and a hard
timeout. Direct executable probing is disabled on platforms without that
sandbox. Cached misses expire after one day; descriptions expire after 30 days
or immediately when the executable path, identity, size, mode, or timestamps
change.

Planned:

- Structured option, subcommand, and enum candidates.
- Filesystem segments.
- Optional project-aware providers.

See [`docs/architecture.md`](docs/architecture.md) and
[`docs/roadmap.md`](docs/roadmap.md).

# Architecture

## Goals

Aster must remain correct and responsive with many shells open across tmux panes
and SSH sessions. Shell processes do not load the history corpus. They send
small requests to one daemon on the current host.

```text
Zsh / future PTY UI
        |
        +-- forked native completion capture
        |
        | versioned request over Unix socket
        v
    aster daemon
        |
        +-- history provider (highest priority)
        +-- lazy man/help description workers
        +-- filesystem and dynamic providers (planned)
        |
        v
  SQLite shared state
```

## Shared State

The daemon owns a primary SQLite connection configured for WAL mode and a
bounded busy timeout. History imports use a separate short-lived connection so
file parsing and bulk insertion do not hold the completion-store mutex. All tmux
panes on a host use the same socket and database. SSH sessions use the daemon on
the remote host, avoiding accidental transfer of command history between
machines.

Command descriptions use a separate owner-private JSON cache. Entries are keyed
by resolved executable path, device, inode, size, mode, modification time, and
change time. This keeps cache loading and persistence independent of the history
database and its completion lock.

The shell sends only command text, working directory, timestamp, session ID, and
exit code. Aster does not collect stdout, stderr, environment values, or command
dependency graphs.

Imported history is tracked by canonical path, size, and modification time. An
unchanged history file is not reparsed when another shell starts.

## Completion Tiers

Providers are queried in strict order:

1. Exact-prefix command history.
2. Append-safe candidates from the active Zsh completion system.
3. Cached installed-command inventory and known shell builtins for the first
   token.
4. Parsed and cached executable help for descriptions and future structured
   candidates.
5. Explicit daemon-side dynamic providers.
6. No result.

Lower tiers may fill unused menu capacity but never outrank a higher tier. No
broad command-intent model is in the interactive path.

## Conservative Editing

Every response includes:

- The complete display candidate.
- A short description and semantic candidate kind.
- The complete insertion suffix.
- The smaller text accepted by one completion-key press.
- The byte range the frontend may replace.
- Candidate provenance.

The initial engine only completes at the end of the buffer. It abstains from
mid-line edits until a shell-aware parser can produce safe replacement ranges.

The Zsh frontend refreshes an inline candidate menu when the command buffer
changes and highlights the highest-ranked candidate. It owns `POSTDISPLAY` while
suggestions are active, so the ghost text and bordered menu participate in ZLE's
normal multiline layout instead of being printed into terminal history. ZLE
`region_highlight` entries style matches, provenance, borders, and the selected
row using validated colors from the UI configuration. Ctrl-N and Ctrl-K move
the selection; the configured completion key accepts one segment and
immediately queries again. Enter is never rebound. Normal Tab clears stale
Aster display state before delegating to the widget it replaced. These controls
delegate to their previous ZLE widgets whenever the menu is closed.

Aster memo-tags only its own highlight regions. It preserves foreign regions
across asynchronous FD callbacks and restores the request buffer before redraw,
so syntax-highlighting plugins retain ownership of command-token colors.

The menu is anchored from a configurable prompt offset plus the live ZLE cursor
and clamped before the terminal's final column. It renders a bounded
viewport with scrolloff, a position counter, semantic icons, descriptions, and
key hints; the candidate order always remains the daemon's ranking order.

Character-editing widgets only update a request snapshot and dirty flag. A
prompt-lifetime ZLE ticker debounces those changes and launches completion in a
background process; an fd handler discards superseded work and asks ZLE to apply
only the current response. No process launch, database access, or socket round
trip occurs on the character-input path.

Unresolved command descriptions are also excluded from that path. A completion
request performs only an in-memory cache lookup and nonblocking bounded enqueue.
Two daemon workers inspect only visible unresolved commands, preferring the
exact man page before a direct `--help` probe. Man and help processes have
owner-private bounded output, process-group timeouts, and cleared pager/color
state. On macOS, direct executable probing additionally runs through
`sandbox-exec` with network and writes denied; platforms without an implemented
sandbox use man pages only. Parsed descriptions are reduced to one control-free
line before being cached or sent to Zsh.

Candidates report whether description discovery is pending. After receiving a
pending result, Zsh polls the unchanged request about every 150 ms, only one
request at a time, until the workers settle it. Polling preserves the selected
candidate and stops immediately when no visible description remains pending.
The legacy five-field Zsh stream remains available for already-running shells;
newly initialized shells use the versioned six-field stream.

Native Zsh completions are shell-local and therefore never generated by the
daemon. After about 120 ms idle on a changed buffer, Aster invokes a completion
widget that immediately forks a child completion context. The child runs the
shell's configured `_main_complete`, wraps ordinary `compadd` calls, and returns
a bounded list over a private pipe. The parent ZLE does not wait for completion
functions or their subprocesses. A new edit closes the pipe and cancels the
child; a 1.5-second ticker deadline handles completion functions that do not
finish.

The frontend accepts only literal append operations at end-of-line. Matches
that need quoting, hidden prefixes, ignored prefixes, in-word replacement, or
other native edit semantics are discarded, while normal Tab remains delegated
to Zsh for exact behavior. Frontend merging keeps daemon history first, then
native matches, then daemon command inventory, deduplicating by full display
text and preserving the current selection across asynchronous updates.

At shell initialization, the frontend checks `locale charmap`. UTF-8 locales use
the bordered glyph UI; other locales use single-byte ASCII borders, markers,
icons, separators, and truncation characters so ZLE does not expose encoded
bytes as `<e2><...>` sequences in SSH or container sessions.

## tmux Titles

Aster owns the per-pane title only while Zsh is idle at the prompt. The Zsh
integration sets `pane_title` to `aster` during `precmd`, changes it to the
submitted command during `preexec`, and lets applications replace it while they
run. `ASTER_TMUX_SHELL_TITLE` can customize the idle title.

This uses `tmux select-pane -T`, not `rename-window`, so Aster does not disable or
replace tmux's automatic window naming. A future PTY-based interface must follow
the same lifecycle and must never leave panes titled `aster` while another
foreground command is active.

The dotfile tmux integration sets `automatic-rename-format` to `#{pane_title}`.
This lets status themes that display the automatic window name show `aster`
while Zsh is idle and the foreground command while it runs, without placing a
wrapper process between tmux and Zsh.

## Daemon Lifecycle

Client commands first attempt the Unix socket. When unavailable, the client
starts `aster daemon` in a new process session and waits for a successful ping.
Concurrent startup attempts are serialized by an owner-private lock held for
the daemon lifetime. Only the lock owner may inspect or remove a stale socket
and bind the endpoint.

The socket and state directories are owner-only. A stale socket left by an
unclean shutdown is removed only after a connection attempt proves no daemon is
listening.

## Configuration

Configuration is strict TOML with unknown fields rejected. Paths follow XDG
locations by default and have explicit `ASTER_*` overrides. The initial daemon
loads configuration at startup; live config reload is planned.

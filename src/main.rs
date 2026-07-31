use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{io, io::Read, io::Write};

use anyhow::{Context, Result, bail};
use aster::client;
use aster::config::{Paths, Settings, completion_key_sequence};
use aster::daemon;
use aster::protocol::{Request, Response};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "aster", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the shared per-host daemon in the foreground.
    Daemon,
    /// Record a completed command in shared history.
    Record {
        #[arg(long, required_unless_present = "stdin", conflicts_with = "stdin")]
        command: Option<String>,
        #[arg(long)]
        stdin: bool,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        exit_code: i32,
        #[arg(long, default_value = "unknown")]
        session: String,
    },
    /// Return conservative history completions for the current buffer.
    Complete {
        #[arg(long, required_unless_present = "stdin", conflicts_with = "stdin")]
        buffer: Option<String>,
        #[arg(long)]
        stdin: bool,
        #[arg(long)]
        cursor: usize,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
    },
    /// Import an existing Zsh history file into shared state.
    ImportHistory {
        #[arg(long)]
        file: PathBuf,
    },
    /// Print shell integration code and initialize the default config.
    Init {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Verify config, storage, and daemon connectivity.
    Doctor,
    /// Stop the shared daemon on this host.
    Stop,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Json,
    Insert,
    Zsh,
    ZshV2,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Shell {
    Zsh,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("aster: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::discover()?;

    match cli.command {
        Command::Daemon => {
            let settings = Settings::load(&paths.config_file)?;
            daemon::serve(paths, settings)
        }
        Command::Record {
            command,
            stdin,
            cwd,
            exit_code,
            session,
        } => {
            expect_success(client::request(
                &paths,
                Request::Record {
                    command: read_input(command, stdin, 64 * 1024)?,
                    cwd: cwd.to_string_lossy().into_owned(),
                    exit_code,
                    observed_at_ms: now_ms(),
                    session_id: session,
                },
            )?)?;
            Ok(())
        }
        Command::Complete {
            buffer,
            stdin,
            cursor,
            cwd,
            limit,
            format,
        } => {
            let response = client::request_idempotent(
                &paths,
                Request::Complete {
                    buffer: read_input(buffer, stdin, 64 * 1024)?,
                    cursor_byte: cursor,
                    cwd: cwd.to_string_lossy().into_owned(),
                    limit,
                },
            )?;
            match response {
                Response::Completion(completion) => match format {
                    OutputFormat::Json => println!("{}", serde_json::to_string(&completion)?),
                    OutputFormat::Insert => {
                        if let Some(candidate) = completion.candidates.first() {
                            print!("{}", candidate.accept_text);
                        }
                    }
                    OutputFormat::Zsh | OutputFormat::ZshV2 => {
                        let stdout = io::stdout();
                        let mut output = stdout.lock();
                        for candidate in completion.candidates {
                            let source = match candidate.source {
                                aster::protocol::CandidateSource::History => "history",
                                aster::protocol::CandidateSource::Command => "command",
                            };
                            let kind = match candidate.kind {
                                aster::protocol::CandidateKind::History => "history",
                                aster::protocol::CandidateKind::Command => "command",
                            };
                            for field in [
                                candidate.accept_text.as_str(),
                                candidate.display.as_str(),
                                candidate.description.as_str(),
                                kind,
                                source,
                            ] {
                                output.write_all(field.as_bytes())?;
                                output.write_all(&[0])?;
                            }
                            if matches!(format, OutputFormat::ZshV2) {
                                output.write_all(if candidate.description_pending {
                                    b"true"
                                } else {
                                    b"false"
                                })?;
                                output.write_all(&[0])?;
                            }
                        }
                    }
                },
                Response::Error { message } => bail!(message),
                response => bail!("unexpected daemon response: {response:?}"),
            }
            Ok(())
        }
        Command::ImportHistory { file } => {
            let file = file
                .canonicalize()
                .with_context(|| format!("failed to resolve history file {}", file.display()))?;
            match client::request(
                &paths,
                Request::ImportHistory {
                    path: file.to_string_lossy().into_owned(),
                },
            )? {
                Response::Imported { imported, skipped } => {
                    if !skipped {
                        eprintln!("aster: imported {imported} history entries");
                    }
                    Ok(())
                }
                Response::Error { message } => bail!(message),
                response => bail!("unexpected daemon response: {response:?}"),
            }
        }
        Command::Init { shell: Shell::Zsh } => {
            Settings::write_default(&paths.config_file)?;
            let settings = Settings::load(&paths.config_file)?;
            print!("{}", zsh_integration(&settings)?);
            Ok(())
        }
        Command::Doctor => doctor(&paths),
        Command::Stop => match client::request_once(&paths, Request::Shutdown)? {
            Response::ShuttingDown => Ok(()),
            Response::Error { message } => bail!(message),
            response => bail!("unexpected daemon response: {response:?}"),
        },
    }
}

fn doctor(paths: &Paths) -> Result<()> {
    Settings::load(&paths.config_file)?;
    let response = client::request(paths, Request::Ping)?;
    let Response::Pong { version } = response else {
        bail!("unexpected daemon response: {response:?}");
    };
    println!("aster {version}");
    println!("config: {}", paths.config_file.display());
    println!("database: {}", paths.database_file.display());
    println!("socket: {}", paths.socket_file.display());
    println!("status: ready");
    Ok(())
}

fn expect_success(response: Response) -> Result<()> {
    match response {
        Response::Recorded => Ok(()),
        Response::Error { message } => bail!(message),
        response => bail!("unexpected daemon response: {response:?}"),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn read_input(value: Option<String>, from_stdin: bool, limit: u64) -> Result<String> {
    if !from_stdin {
        return value.context("missing input value");
    }
    let mut bytes = Vec::new();
    io::stdin().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("stdin input exceeds {limit} bytes");
    }
    String::from_utf8(bytes).context("stdin input is not valid UTF-8")
}

fn zsh_integration(settings: &Settings) -> Result<String> {
    let completion_key = completion_key_sequence(&settings.completion.key)?;
    let completion_key_label = match settings.completion.key.as_str() {
        "ctrl-space" => "Ctrl-Space".to_owned(),
        "shift-tab" => "Shift-Tab".to_owned(),
        "tab" => "Tab".to_owned(),
        key => format!(
            "Ctrl-{}",
            key.trim_start_matches("ctrl-").to_ascii_uppercase()
        ),
    };
    let menu_width = settings.ui.menu_width.to_string();
    let max_visible = settings.ui.max_visible.to_string();
    let max_candidates = settings.completion.max_candidates.to_string();
    Ok(r#"# Aster shell integration
if [[ -o interactive && -z "${ASTER_ZSH_LOADED:-}" ]] && (( $+commands[aster] )); then
  typeset -g ASTER_ZSH_LOADED=1
  typeset -g _ASTER_COMMAND=""
  typeset -g _ASTER_COMMAND_CWD=""
  typeset -g _ASTER_MENU_ACTIVE=0
  typeset -g _ASTER_MENU_INDEX=1
  typeset -g _ASTER_MENU_START=1
  typeset -g _ASTER_MENU_BUFFER=""
  typeset -g _ASTER_MENU_OWNS_DISPLAY=0
  typeset -g _ASTER_MENU_REQUEST_BUFFER=""
  typeset -g _ASTER_MENU_REQUEST_CWD=""
  typeset -g _ASTER_MENU_REQUEST_CURSOR=0
  typeset -g _ASTER_MENU_REQUEST_FD=-1
  typeset -g _ASTER_MENU_REQUEST_DIRTY=0
  typeset -g _ASTER_MENU_REFRESH_TICKS=0
  typeset -g _ASTER_MENU_RESTORE_DISPLAY=""
  typeset -g _ASTER_RESTORE_HIGHLIGHTS=0
  typeset -g _ASTER_MENU_TICK_FD=-1
  typeset -g _ASTER_HAS_ZSELECT=0
  typeset -g _ASTER_NATIVE_REQUEST_FD=-1
  typeset -g _ASTER_NATIVE_REQUEST_PID=-1
  typeset -g _ASTER_NATIVE_REQUEST_TICKS=0
  typeset -g _ASTER_NATIVE_START_TICKS=0
  typeset -g _ASTER_NATIVE_REQUESTED=0
  typeset -ga _ASTER_MENU_ACCEPTS=()
  typeset -ga _ASTER_MENU_DISPLAYS=()
  typeset -ga _ASTER_MENU_DESCRIPTIONS=()
  typeset -ga _ASTER_MENU_KINDS=()
  typeset -ga _ASTER_MENU_SOURCES=()
  typeset -ga _ASTER_FOREIGN_HIGHLIGHTS=()
  typeset -ga _ASTER_RENDERED_HIGHLIGHTS=()
  typeset -ga _ASTER_DAEMON_ACCEPTS=()
  typeset -ga _ASTER_DAEMON_DISPLAYS=()
  typeset -ga _ASTER_DAEMON_DESCRIPTIONS=()
  typeset -ga _ASTER_DAEMON_KINDS=()
  typeset -ga _ASTER_DAEMON_SOURCES=()
  typeset -ga _ASTER_NATIVE_ACCEPTS=()
  typeset -ga _ASTER_NATIVE_DISPLAYS=()
  typeset -ga _ASTER_NATIVE_DESCRIPTIONS=()

  _aster_tmux_title() {
    if [[ -n "${TMUX_PANE:-}" ]] && (( $+commands[tmux] )); then
      command tmux select-pane -t "$TMUX_PANE" -T "$1" >/dev/null 2>&1
    fi
    return 0
  }

  _aster_preexec() {
    local command_name="${${1%%[[:space:]]*}:t}"
    _ASTER_COMMAND="$1"
    _ASTER_COMMAND_CWD="$PWD"
    _aster_tmux_title "${command_name:-command}"
    return 0
  }

  _aster_precmd() {
    local exit_code=$?
    if [[ -n "$_ASTER_COMMAND" ]]; then
      local session_id="${HOST:-unknown}:${TMUX_PANE:-${SSH_TTY:-local}}:$$"
      print -rn -- "$_ASTER_COMMAND" | command aster record \
        --stdin \
        --cwd "$_ASTER_COMMAND_CWD" \
        --exit-code "$exit_code" \
        --session "$session_id" >/dev/null 2>&1 &!
      _ASTER_COMMAND=""
      _ASTER_COMMAND_CWD=""
    fi
    _aster_tmux_title "${ASTER_TMUX_SHELL_TITLE:-aster}"
    return 0
  }

  _aster_menu_cancel_query() {
    local fd="$_ASTER_MENU_REQUEST_FD"
    if (( fd >= 0 )); then
      zle -F "$fd" 2>/dev/null
      exec {fd}<&-
    fi
    _ASTER_MENU_REQUEST_FD=-1
  }

  _aster_native_cancel() {
    local fd="$_ASTER_NATIVE_REQUEST_FD"
    if (( fd >= 0 )); then
      zle -F "$fd" 2>/dev/null
      exec {fd}<&-
    fi
    if (( _ASTER_NATIVE_REQUEST_PID > 0 )); then
      kill "$_ASTER_NATIVE_REQUEST_PID" 2>/dev/null
    fi
    _ASTER_NATIVE_REQUEST_FD=-1
    _ASTER_NATIVE_REQUEST_PID=-1
    _ASTER_NATIVE_REQUEST_TICKS=0
    _ASTER_NATIVE_START_TICKS=0
  }

  _aster_menu_cancel_request() {
    _aster_menu_cancel_query
    _aster_native_cancel
    _ASTER_MENU_REQUEST_BUFFER=""
    _ASTER_MENU_REQUEST_CWD=""
    _ASTER_MENU_REQUEST_CURSOR=0
    _ASTER_MENU_REQUEST_DIRTY=0
    _ASTER_MENU_REFRESH_TICKS=0
    _ASTER_MENU_RESTORE_DISPLAY=""
    _ASTER_NATIVE_REQUESTED=0
    _ASTER_NATIVE_START_TICKS=0
  }

  _aster_menu_clear() {
    local preserve_buffer="${1:-0}"
    _aster_menu_cancel_request
    _ASTER_MENU_ACTIVE=0
    _ASTER_MENU_INDEX=1
    _ASTER_MENU_START=1
    _ASTER_RESTORE_HIGHLIGHTS=0
    (( preserve_buffer )) || _ASTER_MENU_BUFFER=""
    _ASTER_MENU_ACCEPTS=()
    _ASTER_MENU_DISPLAYS=()
    _ASTER_MENU_DESCRIPTIONS=()
    _ASTER_MENU_KINDS=()
    _ASTER_MENU_SOURCES=()
    _ASTER_DAEMON_ACCEPTS=()
    _ASTER_DAEMON_DISPLAYS=()
    _ASTER_DAEMON_DESCRIPTIONS=()
    _ASTER_DAEMON_KINDS=()
    _ASTER_DAEMON_SOURCES=()
    _ASTER_NATIVE_ACCEPTS=()
    _ASTER_NATIVE_DISPLAYS=()
    _ASTER_NATIVE_DESCRIPTIONS=()
    if (( _ASTER_MENU_OWNS_DISPLAY )); then
      POSTDISPLAY=""
      region_highlight=( "${(@)region_highlight:#*memo=aster*}" )
      _ASTER_MENU_OWNS_DISPLAY=0
    fi
  }

  _aster_menu_render() {
    local index display description kind icon marker row top bottom fill ghost=""
    local input="${BUFFER:-$_ASTER_MENU_REQUEST_BUFFER}"
    local cursor_position=$CURSOR
    if [[ -z "$BUFFER" && -n "$_ASTER_MENU_REQUEST_BUFFER" ]]; then
      cursor_position=$_ASTER_MENU_REQUEST_CURSOR
    fi
    local box_width=$(( ${COLUMNS:-80} - 2 ))
    (( box_width > __ASTER_UI_MENU_WIDTH__ )) && box_width=__ASTER_UI_MENU_WIDTH__
    if (( box_width < 40 )); then
      POSTDISPLAY=""
      return 0
    fi

    local total=${#_ASTER_MENU_DISPLAYS}
    local window_size=$total
    (( window_size > __ASTER_UI_MAX_VISIBLE__ )) && window_size=__ASTER_UI_MAX_VISIBLE__
    local max_start=$(( total - window_size + 1 ))
    if (( window_size > 2 )); then
      (( _ASTER_MENU_INDEX <= _ASTER_MENU_START && _ASTER_MENU_START > 1 )) && \
        _ASTER_MENU_START=$(( _ASTER_MENU_INDEX - 1 ))
      (( _ASTER_MENU_INDEX >= _ASTER_MENU_START + window_size - 1 )) && \
        _ASTER_MENU_START=$(( _ASTER_MENU_INDEX - window_size + 2 ))
    else
      (( _ASTER_MENU_INDEX < _ASTER_MENU_START )) && _ASTER_MENU_START=$_ASTER_MENU_INDEX
      (( _ASTER_MENU_INDEX >= _ASTER_MENU_START + window_size )) && \
        _ASTER_MENU_START=$(( _ASTER_MENU_INDEX - window_size + 1 ))
    fi
    (( _ASTER_MENU_START < 1 )) && _ASTER_MENU_START=1
    (( _ASTER_MENU_START > max_start )) && _ASTER_MENU_START=$max_start
    local end=$(( _ASTER_MENU_START + window_size - 1 ))

    local description_width=24
    (( box_width < 64 )) && description_width=16
    local title_width=$(( box_width - description_width - 9 ))
    local top_prefix="╭─"
    local count=" $_ASTER_MENU_INDEX/$total "
    printf -v fill '%*s' "$(( box_width - ${#top_prefix} - ${#count} - 1 ))" ""
    fill="${fill// /─}"
    top="${top_prefix}${fill}${count}╮"
    local footer=" Ctrl-N/K move · __ASTER_COMPLETION_KEY_LABEL__ accept "
    local bottom_prefix="╰─${footer}"
    printf -v fill '%*s' "$(( box_width - ${#bottom_prefix} - 1 ))" ""
    fill="${fill// /─}"
    bottom="${bottom_prefix}${fill}╯"

    local indent=$(( __ASTER_UI_PROMPT_OFFSET__ + cursor_position ))
    local max_indent=$(( ${COLUMNS:-80} - box_width - 1 ))
    (( indent > max_indent )) && indent=$max_indent
    (( indent < 0 )) && indent=0
    local padding
    printf -v padding '%*s' "$indent" ""

    local selected="$_ASTER_MENU_DISPLAYS[$_ASTER_MENU_INDEX]"
    if (( cursor_position == ${#input} )) && [[ -n "$input" && "$selected" == "$input"* ]]; then
      ghost="${selected[${#input}+1,-1]}"
    fi

    region_highlight=( "${(@)region_highlight:#*memo=aster*}" )
    POSTDISPLAY="$ghost"
    local buffer_end=${#input}
    if [[ -n "$ghost" ]]; then
      region_highlight+=("$buffer_end $(( buffer_end + ${#ghost} )) fg=__ASTER_UI_GHOST__ memo=aster")
    fi

    local line_start=$(( ${#input} + ${#POSTDISPLAY} + 1 + indent ))
    POSTDISPLAY+=$'\n'"${padding}${top}"
    region_highlight+=("$line_start $(( line_start + ${#top} )) fg=__ASTER_UI_BORDER__ memo=aster")

    for (( index = _ASTER_MENU_START; index <= end; index++ )); do
      display="$_ASTER_MENU_DISPLAYS[$index]"
      if (( ${#display} > title_width )); then
        display="${display[1,$(( title_width - 1 ))]}…"
      fi
      description="$_ASTER_MENU_DESCRIPTIONS[$index]"
      if (( ${#description} > description_width )); then
        description="${description[1,$(( description_width - 1 ))]}…"
      fi
      kind="$_ASTER_MENU_KINDS[$index]"
      case "$kind" in
        history) icon="↺" ;;
        command) icon="❯" ;;
        native) icon="⇥" ;;
        *) icon="·" ;;
      esac
      marker="  "
      (( index == _ASTER_MENU_INDEX )) && marker="▶ "
      printf -v row '│ %s%s %-*s %-*s │' \
        "$marker" "$icon" "$title_width" "$display" \
        "$description_width" "$description"

      line_start=$(( ${#input} + ${#POSTDISPLAY} + 1 + indent ))
      POSTDISPLAY+=$'\n'"${padding}${row}"
      if (( index == _ASTER_MENU_INDEX )); then
        region_highlight+=("$(( line_start + 1 )) $(( line_start + ${#row} - 1 )) bg=__ASTER_UI_SELECTED_BACKGROUND__,fg=__ASTER_UI_SELECTED_TEXT__ memo=aster")
        region_highlight+=("$(( line_start + 2 )) $(( line_start + 3 )) bg=__ASTER_UI_SELECTED_BACKGROUND__,fg=__ASTER_UI_ACCENT__,bold memo=aster")
        region_highlight+=("$(( line_start + 4 )) $(( line_start + 5 )) bg=__ASTER_UI_SELECTED_BACKGROUND__,fg=__ASTER_UI_ACCENT__ memo=aster")
      else
        region_highlight+=("$(( line_start + 1 )) $(( line_start + ${#row} - 1 )) fg=__ASTER_UI_TEXT__ memo=aster")
        region_highlight+=("$(( line_start + 4 )) $(( line_start + 5 )) fg=__ASTER_UI_MUTED__ memo=aster")
      fi
      region_highlight+=("$line_start $(( line_start + 1 )) fg=__ASTER_UI_BORDER__ memo=aster")
      region_highlight+=("$(( line_start + ${#row} - 1 )) $(( line_start + ${#row} )) fg=__ASTER_UI_BORDER__ memo=aster")

      local match_length=${#input}
      (( match_length > title_width )) && match_length=$title_width
      if (( match_length > 0 )); then
        if (( index == _ASTER_MENU_INDEX )); then
          region_highlight+=("$(( line_start + 6 )) $(( line_start + 6 + match_length )) bg=__ASTER_UI_SELECTED_BACKGROUND__,fg=__ASTER_UI_SELECTED_TEXT__,bold memo=aster")
        else
          region_highlight+=("$(( line_start + 6 )) $(( line_start + 6 + match_length )) fg=__ASTER_UI_SELECTED_TEXT__,bold memo=aster")
        fi
      fi
      local description_start=$(( line_start + 6 + title_width + 1 ))
      if (( index == _ASTER_MENU_INDEX )); then
        region_highlight+=("$description_start $(( description_start + description_width )) bg=__ASTER_UI_SELECTED_BACKGROUND__,fg=__ASTER_UI_SELECTED_SOURCE__ memo=aster")
      else
        region_highlight+=("$description_start $(( description_start + description_width )) fg=__ASTER_UI_MUTED__ memo=aster")
      fi
    done

    line_start=$(( ${#input} + ${#POSTDISPLAY} + 1 + indent ))
    POSTDISPLAY+=$'\n'"${padding}${bottom}"
    region_highlight+=("$line_start $(( line_start + ${#bottom} )) fg=__ASTER_UI_BORDER__ memo=aster")
    _ASTER_MENU_OWNS_DISPLAY=1
  }

  _aster_menu_query() {
    local LC_ALL=C cursor_byte accept display description kind source
    _aster_menu_cancel_request
    cursor_byte=${#LBUFFER}
    _ASTER_MENU_BUFFER="$BUFFER"
    _ASTER_MENU_ACCEPTS=()
    _ASTER_MENU_DISPLAYS=()
    _ASTER_MENU_DESCRIPTIONS=()
    _ASTER_MENU_KINDS=()
    _ASTER_MENU_SOURCES=()
    while IFS= read -r -d '' accept &&
          IFS= read -r -d '' display &&
          IFS= read -r -d '' description &&
          IFS= read -r -d '' kind &&
          IFS= read -r -d '' source; do
      _ASTER_MENU_ACCEPTS+=("$accept")
      _ASTER_MENU_DISPLAYS+=("$display")
      _ASTER_MENU_DESCRIPTIONS+=("$description")
      _ASTER_MENU_KINDS+=("$kind")
      _ASTER_MENU_SOURCES+=("$source")
    done < <(print -rn -- "$BUFFER" | command aster complete \
      --stdin \
      --cursor "$cursor_byte" \
      --cwd "$PWD" \
      --format zsh 2>/dev/null)
    if (( ${#_ASTER_MENU_ACCEPTS} == 0 )); then
      _aster_menu_clear 1
      return 1
    fi
    _ASTER_MENU_ACTIVE=1
    _ASTER_MENU_INDEX=1
    _ASTER_MENU_START=1
    _aster_menu_render
    return 0
  }

  _aster_menu_accept() {
    (( CURSOR == ${#BUFFER} )) || return 1
    LBUFFER+="${_ASTER_MENU_ACCEPTS[$_ASTER_MENU_INDEX]}"
    POSTDISPLAY=""
    _aster_menu_clear
    _ASTER_MENU_BUFFER="$BUFFER"
    _aster_menu_schedule
  }

  _aster_menu_refresh() {
    _aster_menu_clear
    _ASTER_MENU_BUFFER="$BUFFER"
    if [[ -n "$BUFFER" ]] && (( CURSOR == ${#BUFFER} )); then
      _aster_menu_schedule
    fi
  }

  _aster_menu_schedule() {
    local LC_ALL=C
    _aster_menu_cancel_query
    _aster_native_cancel
    _ASTER_MENU_REQUEST_BUFFER="$BUFFER"
    _ASTER_MENU_REQUEST_CWD="$PWD"
    _ASTER_MENU_REQUEST_CURSOR=${#LBUFFER}
    _ASTER_MENU_REQUEST_DIRTY=1
    _ASTER_NATIVE_REQUESTED=0
    _ASTER_NATIVE_START_TICKS=0
    _ASTER_DAEMON_ACCEPTS=()
    _ASTER_DAEMON_DISPLAYS=()
    _ASTER_DAEMON_DESCRIPTIONS=()
    _ASTER_DAEMON_KINDS=()
    _ASTER_DAEMON_SOURCES=()
    _ASTER_NATIVE_ACCEPTS=()
    _ASTER_NATIVE_DISPLAYS=()
    _ASTER_NATIVE_DESCRIPTIONS=()
  }

  _aster_menu_publish() {
    local selected="${_ASTER_MENU_DISPLAYS[$_ASTER_MENU_INDEX]}"
    local index display
    local -a redraw_hooks apply_hooks
    local limit=$(( __ASTER_COMPLETION_MAX_CANDIDATES__ * 2 ))
    _ASTER_MENU_ACCEPTS=()
    _ASTER_MENU_DISPLAYS=()
    _ASTER_MENU_DESCRIPTIONS=()
    _ASTER_MENU_KINDS=()
    _ASTER_MENU_SOURCES=()

    for (( index = 1; index <= ${#_ASTER_DAEMON_ACCEPTS}; index++ )); do
      [[ "${_ASTER_DAEMON_SOURCES[$index]}" == history ]] || continue
      display="${_ASTER_DAEMON_DISPLAYS[$index]}"
      [[ -n "$display" ]] && (( ! ${_ASTER_MENU_DISPLAYS[(Ie)$display]} )) || continue
      _ASTER_MENU_ACCEPTS+=("${_ASTER_DAEMON_ACCEPTS[$index]}")
      _ASTER_MENU_DISPLAYS+=("$display")
      _ASTER_MENU_DESCRIPTIONS+=("${_ASTER_DAEMON_DESCRIPTIONS[$index]}")
      _ASTER_MENU_KINDS+=("${_ASTER_DAEMON_KINDS[$index]}")
      _ASTER_MENU_SOURCES+=("${_ASTER_DAEMON_SOURCES[$index]}")
      (( ${#_ASTER_MENU_ACCEPTS} >= limit )) && break
    done

    if (( ${#_ASTER_MENU_ACCEPTS} < limit )); then
      for (( index = 1; index <= ${#_ASTER_NATIVE_ACCEPTS}; index++ )); do
        display="${_ASTER_NATIVE_DISPLAYS[$index]}"
        [[ -n "$display" ]] && (( ! ${_ASTER_MENU_DISPLAYS[(Ie)$display]} )) || continue
        _ASTER_MENU_ACCEPTS+=("${_ASTER_NATIVE_ACCEPTS[$index]}")
        _ASTER_MENU_DISPLAYS+=("$display")
        _ASTER_MENU_DESCRIPTIONS+=("${_ASTER_NATIVE_DESCRIPTIONS[$index]}")
        _ASTER_MENU_KINDS+=("native")
        _ASTER_MENU_SOURCES+=("native")
        (( ${#_ASTER_MENU_ACCEPTS} >= limit )) && break
      done
    fi

    if (( ${#_ASTER_MENU_ACCEPTS} < limit )); then
      for (( index = 1; index <= ${#_ASTER_DAEMON_ACCEPTS}; index++ )); do
        [[ "${_ASTER_DAEMON_SOURCES[$index]}" == history ]] && continue
        display="${_ASTER_DAEMON_DISPLAYS[$index]}"
        [[ -n "$display" ]] && (( ! ${_ASTER_MENU_DISPLAYS[(Ie)$display]} )) || continue
        _ASTER_MENU_ACCEPTS+=("${_ASTER_DAEMON_ACCEPTS[$index]}")
        _ASTER_MENU_DISPLAYS+=("$display")
        _ASTER_MENU_DESCRIPTIONS+=("${_ASTER_DAEMON_DESCRIPTIONS[$index]}")
        _ASTER_MENU_KINDS+=("${_ASTER_DAEMON_KINDS[$index]}")
        _ASTER_MENU_SOURCES+=("${_ASTER_DAEMON_SOURCES[$index]}")
        (( ${#_ASTER_MENU_ACCEPTS} >= limit )) && break
      done
    fi

    _ASTER_MENU_RESTORE_DISPLAY="$selected"
    zstyle -a zle-line-pre-redraw widgets redraw_hooks
    apply_hooks=( "${(@)redraw_hooks:#*:_zsh_highlight__zle-line-pre-redraw}" )
    if (( ${#apply_hooks} != ${#redraw_hooks} )); then
      zstyle zle-line-pre-redraw widgets "${apply_hooks[@]}"
      {
        zle aster-menu-apply
      } always {
        zstyle zle-line-pre-redraw widgets "${redraw_hooks[@]}"
      }
    else
      zle aster-menu-apply
    fi
    region_highlight=( "${_ASTER_RENDERED_HIGHLIGHTS[@]}" )
  }

  _aster_menu_request_ready() {
    local fd="$1" accept display description kind source pending
    local any_pending=0
    local -a accepts displays descriptions kinds sources
    if (( fd != _ASTER_MENU_REQUEST_FD )); then
      zle -F "$fd" 2>/dev/null
      exec {fd}<&-
      return 0
    fi

    zle -F "$fd"
    BUFFER="$_ASTER_MENU_REQUEST_BUFFER"
    CURSOR=$_ASTER_MENU_REQUEST_CURSOR
    while IFS= read -r -u "$fd" -d '' accept &&
          IFS= read -r -u "$fd" -d '' display &&
          IFS= read -r -u "$fd" -d '' description &&
          IFS= read -r -u "$fd" -d '' kind &&
          IFS= read -r -u "$fd" -d '' source &&
          IFS= read -r -u "$fd" -d '' pending; do
      accepts+=("$accept")
      displays+=("$display")
      descriptions+=("$description")
      kinds+=("$kind")
      sources+=("$source")
      [[ "$pending" == true ]] && any_pending=1
    done
    exec {fd}<&-

    _ASTER_MENU_REQUEST_FD=-1
    _ASTER_DAEMON_ACCEPTS=("${accepts[@]}")
    _ASTER_DAEMON_DISPLAYS=("${displays[@]}")
    _ASTER_DAEMON_DESCRIPTIONS=("${descriptions[@]}")
    _ASTER_DAEMON_KINDS=("${kinds[@]}")
    _ASTER_DAEMON_SOURCES=("${sources[@]}")
    (( any_pending )) && _ASTER_MENU_REFRESH_TICKS=5 || _ASTER_MENU_REFRESH_TICKS=0
    _aster_menu_publish
  }

  _aster_native_request_ready() {
    local fd="$1" accept display description
    local -a accepts displays descriptions
    if (( fd != _ASTER_NATIVE_REQUEST_FD )); then
      zle -F "$fd" 2>/dev/null
      exec {fd}<&-
      return 0
    fi

    zle -F "$fd"
    BUFFER="$_ASTER_MENU_REQUEST_BUFFER"
    CURSOR=$_ASTER_MENU_REQUEST_CURSOR
    while IFS= read -r -u "$fd" -d '' accept &&
          IFS= read -r -u "$fd" -d '' display &&
          IFS= read -r -u "$fd" -d '' description; do
      accepts+=("$accept")
      displays+=("$display")
      descriptions+=("$description")
    done
    exec {fd}<&-
    _ASTER_NATIVE_REQUEST_FD=-1
    _ASTER_NATIVE_REQUEST_PID=-1
    _ASTER_NATIVE_REQUEST_TICKS=0

    _ASTER_NATIVE_ACCEPTS=("${accepts[@]}")
    _ASTER_NATIVE_DISPLAYS=("${displays[@]}")
    _ASTER_NATIVE_DESCRIPTIONS=("${descriptions[@]}")
    _aster_menu_publish
  }

  _aster_native_capture_widget() {
    local fd
    exec {fd}< <(
      local original_lbuffer="$LBUFFER"
      local -a accepts displays descriptions

      compadd() {
        local argument status current_prefix="$PREFIX"
        local added_prefix="" hidden_prefix="" ignored_prefix=""
        local added_suffix="" hidden_suffix="" ignored_suffix=""
        local index=1
        for argument in "$@"; do
          if [[ "$argument" == -A || "$argument" == -O || "$argument" == -D ]]; then
            builtin compadd "$@"
            return
          fi
        done
        while (( index <= $# )); do
          argument="${@[index]}"
          case "$argument" in
            --) break ;;
            -P|-p|-i|-S|-s|-I)
              (( index++ ))
              (( index <= $# )) || break
              case "$argument" in
                -P) added_prefix="${@[index]}" ;;
                -p) hidden_prefix="${@[index]}" ;;
                -i) ignored_prefix="${@[index]}" ;;
                -S) added_suffix="${@[index]}" ;;
                -s) hidden_suffix="${@[index]}" ;;
                -I) ignored_suffix="${@[index]}" ;;
              esac
              ;;
          esac
          (( index++ ))
        done

        local -a generated
        generated=("${(@0)$(
          local -a matches
          builtin compadd -A matches "$@" 2>/dev/null
          local match
          for match in "${matches[@]}"; do
            print -rn -- "$match"$'\0'
          done
        )}")

        if [[ -z "$IPREFIX$ISUFFIX$QIPREFIX$QISUFFIX$SUFFIX$added_prefix$hidden_prefix$ignored_prefix" ]]; then
          local base_length=$(( ${#original_lbuffer} - ${#current_prefix} ))
          local base="" match full accept
          (( base_length > 0 )) && base="${original_lbuffer[1,$base_length]}"
          if (( base_length >= 0 )) &&
             [[ -z "$current_prefix" ||
                "${original_lbuffer[$(( base_length + 1 )),-1]}" == "$current_prefix" ]]; then
            for match in "${generated[@]}"; do
              (( ${#displays} < __ASTER_COMPLETION_MAX_CANDIDATES__ )) || break
              [[ -n "$match" && "${(q)match}" == "$match" ]] || continue
              [[ -z "$current_prefix" ||
                 "${match[1,${#current_prefix}]}" == "$current_prefix" ]] || continue
              full="${base}${match}${hidden_suffix}${added_suffix}${ignored_suffix}"
              [[ "$full" == "$original_lbuffer"* && "$full" != "$original_lbuffer" ]] || continue
              (( ${displays[(Ie)$full]} )) && continue
              accept="${full[${#original_lbuffer}+1,-1]}"
              [[ -n "$accept" ]] || continue
              accepts+=("$accept")
              displays+=("$full")
              descriptions+=("Zsh completion")
            done
          fi
        fi

        builtin compadd "$@"
        status=$?
        return status
      }

      _main_complete >/dev/null 2>&1
      local index
      for (( index = 1; index <= ${#accepts}; index++ )); do
        printf '%s\0%s\0%s\0' \
          "${accepts[$index]}" \
          "${displays[$index]}" \
          "${descriptions[$index]}"
      done
    )
    _ASTER_NATIVE_REQUEST_FD="$fd"
    _ASTER_NATIVE_REQUEST_PID=$!
    _ASTER_NATIVE_REQUEST_TICKS=0
    zle -F "$fd" _aster_native_request_ready
    compstate[insert]=''
    compstate[list]=''
  }

  _aster_menu_apply_result() {
    local -a rendered_highlights
    if (( ${#_ASTER_MENU_ACCEPTS} == 0 )); then
      _ASTER_MENU_ACTIVE=0
      POSTDISPLAY=""
      region_highlight=( "${_ASTER_FOREIGN_HIGHLIGHTS[@]}" )
      rendered_highlights=( "${region_highlight[@]}" )
      _ASTER_RENDERED_HIGHLIGHTS=( "${rendered_highlights[@]}" )
      _ASTER_RESTORE_HIGHLIGHTS=1
      zle -R
      region_highlight=( "${rendered_highlights[@]}" )
      return 0
    fi
    _ASTER_MENU_ACTIVE=1
    _ASTER_MENU_INDEX=1
    if [[ -n "$_ASTER_MENU_RESTORE_DISPLAY" ]]; then
      local index
      for (( index = 1; index <= ${#_ASTER_MENU_DISPLAYS}; index++ )); do
        if [[ "${_ASTER_MENU_DISPLAYS[$index]}" == "$_ASTER_MENU_RESTORE_DISPLAY" ]]; then
          _ASTER_MENU_INDEX=$index
          break
        fi
      done
    fi
    _ASTER_MENU_RESTORE_DISPLAY=""
    _ASTER_MENU_START=$_ASTER_MENU_INDEX
    _ASTER_MENU_BUFFER="$_ASTER_MENU_REQUEST_BUFFER"
    region_highlight=( "${_ASTER_FOREIGN_HIGHLIGHTS[@]}" )
    _aster_menu_render
    rendered_highlights=( "${region_highlight[@]}" )
    _ASTER_RENDERED_HIGHLIGHTS=( "${rendered_highlights[@]}" )
    _ASTER_RESTORE_HIGHLIGHTS=1
    zle -R
    region_highlight=( "${rendered_highlights[@]}" )
  }

  _aster_menu_start_ticker() {
    (( _ASTER_MENU_TICK_FD >= 0 )) && return 0
    local fd
    exec {fd}< <(
      if (( _ASTER_HAS_ZSELECT )); then
        while true; do
          zselect -t 3
          print -r -- tick || exit
        done
      else
        while sleep 0.03; do
          print -r -- tick || exit
        done
      fi
    )
    _ASTER_MENU_TICK_FD="$fd"
    zle -F "$fd" _aster_menu_tick
  }

  _aster_menu_stop_ticker() {
    local fd="$_ASTER_MENU_TICK_FD"
    if (( fd >= 0 )); then
      zle -F "$fd" 2>/dev/null
      exec {fd}<&-
      _ASTER_MENU_TICK_FD=-1
    fi
  }

  _aster_menu_tick() {
    local fd="$1" tick query_fd buffer cwd cursor
    if (( fd != _ASTER_MENU_TICK_FD )) || ! IFS= read -r -u "$fd" tick; then
      zle -F "$fd" 2>/dev/null
      exec {fd}<&-
      _ASTER_MENU_TICK_FD=-1
      return 0
    fi
    if (( _ASTER_NATIVE_REQUEST_FD >= 0 )); then
      (( _ASTER_NATIVE_REQUEST_TICKS++ ))
      if (( _ASTER_NATIVE_REQUEST_TICKS > 50 )); then
        _aster_native_cancel
      fi
    fi
    if (( ! _ASTER_NATIVE_REQUESTED )) && [[ -n "$_ASTER_MENU_REQUEST_BUFFER" ]]; then
      (( _ASTER_NATIVE_START_TICKS++ ))
      if (( _ASTER_NATIVE_START_TICKS >= 4 )); then
        _ASTER_NATIVE_REQUESTED=1
        if (( $+functions[_main_complete] )); then
          zle aster-native-capture
        fi
      fi
    fi
    if (( ! _ASTER_MENU_REQUEST_DIRTY )); then
      (( _ASTER_MENU_REFRESH_TICKS > 0 )) || return 0
      (( _ASTER_MENU_REFRESH_TICKS-- ))
      (( _ASTER_MENU_REFRESH_TICKS == 0 )) || return 0
      _ASTER_MENU_REQUEST_DIRTY=1
    fi

    _ASTER_MENU_REQUEST_DIRTY=0
    buffer="$_ASTER_MENU_REQUEST_BUFFER"
    cwd="$_ASTER_MENU_REQUEST_CWD"
    cursor="$_ASTER_MENU_REQUEST_CURSOR"
    if [[ -z "$buffer" ]]; then
      return 0
    fi

    _aster_menu_cancel_query
    exec {query_fd}< <(print -rn -- "$buffer" | command aster complete \
      --stdin \
      --cursor "$cursor" \
      --cwd "$cwd" \
      --format zsh-v2 2>/dev/null)
    _ASTER_MENU_REQUEST_FD="$query_fd"
    zle -F "$query_fd" _aster_menu_request_ready
  }

  _aster_menu_line_init() {
    _aster_menu_start_ticker
  }

  _aster_menu_line_finish() {
    _aster_menu_clear
    _aster_menu_stop_ticker
  }

  _aster_self_insert() {
    zle _aster-native-self-insert
    _aster_menu_refresh
  }

  _aster_backward_delete() {
    zle _aster-native-backward-delete
    _aster_menu_refresh
  }

  _aster_bracketed_paste() {
    zle _aster-native-bracketed-paste
    _aster_menu_refresh
  }

  _aster_tab() {
    _aster_menu_clear
    POSTDISPLAY=""
    zle _aster-native-tab
    _ASTER_MENU_BUFFER="$BUFFER"
  }

  _aster_complete() {
    if (( _ASTER_MENU_ACTIVE && CURSOR == ${#BUFFER} )); then
      _aster_menu_accept
    elif (( CURSOR == ${#BUFFER} )) && _aster_menu_query; then
      _aster_menu_accept
    else
      (( _ASTER_MENU_ACTIVE )) && _aster_menu_clear 1
      zle _aster-native-trigger
    fi
  }

  _aster_menu_down() {
    if (( _ASTER_MENU_ACTIVE )); then
      (( _ASTER_MENU_INDEX < ${#_ASTER_MENU_ACCEPTS} )) && (( _ASTER_MENU_INDEX++ ))
      _aster_menu_render
    else
      zle _aster-native-down
      _aster_menu_refresh
    fi
  }

  _aster_menu_up() {
    if (( _ASTER_MENU_ACTIVE )); then
      (( _ASTER_MENU_INDEX > 1 )) && (( _ASTER_MENU_INDEX-- ))
      _aster_menu_render
    else
      zle _aster-native-up
      _aster_menu_refresh
    fi
  }

  _aster_menu_pre_redraw() {
    if (( _ASTER_MENU_ACTIVE )) && [[ "$BUFFER" == "$_ASTER_MENU_BUFFER" ]]; then
      region_highlight=( "${_ASTER_FOREIGN_HIGHLIGHTS[@]}" )
      _ASTER_RESTORE_HIGHLIGHTS=0
    elif (( _ASTER_RESTORE_HIGHLIGHTS )); then
      region_highlight=( "${_ASTER_FOREIGN_HIGHLIGHTS[@]}" )
      _ASTER_RESTORE_HIGHLIGHTS=0
    elif [[ -n "$BUFFER" ]]; then
      _ASTER_FOREIGN_HIGHLIGHTS=( "${(@)region_highlight:#*memo=aster*}" )
    elif [[ -n "$_ASTER_MENU_REQUEST_BUFFER" ]]; then
      region_highlight=( "${_ASTER_FOREIGN_HIGHLIGHTS[@]}" )
    else
      _ASTER_FOREIGN_HIGHLIGHTS=()
    fi
    if [[ "$BUFFER" != "$_ASTER_MENU_BUFFER" ]]; then
      _aster_menu_clear
      _ASTER_MENU_BUFFER="$BUFFER"
      [[ -n "$BUFFER" ]] && (( CURSOR == ${#BUFFER} )) && _aster_menu_schedule
    elif (( CURSOR != ${#BUFFER} )); then
      _aster_menu_cancel_request
    fi
    (( _ASTER_MENU_ACTIVE )) && _aster_menu_render
  }

  autoload -Uz add-zsh-hook
  autoload -Uz add-zle-hook-widget
  zmodload zsh/zselect 2>/dev/null && _ASTER_HAS_ZSELECT=1
  if (( $+functions[_zsh_autosuggest_bind_widgets] )); then
    typeset -g ZSH_AUTOSUGGEST_MANUAL_REBIND=1
    precmd_functions=(${precmd_functions:#_zsh_autosuggest_start})
  fi
  add-zsh-hook preexec _aster_preexec
  add-zsh-hook precmd _aster_precmd
  add-zle-hook-widget line-init _aster_menu_line_init
  add-zle-hook-widget line-finish _aster_menu_line_finish
  add-zle-hook-widget line-pre-redraw _aster_menu_pre_redraw
  preexec_functions=(_aster_preexec ${preexec_functions:#_aster_preexec})
  precmd_functions=(_aster_precmd ${precmd_functions:#_aster_precmd})

  typeset -g _ASTER_PREVIOUS_TRIGGER="${$(bindkey '__ASTER_COMPLETION_KEY__')##* }"
  [[ -z "$_ASTER_PREVIOUS_TRIGGER" || "$_ASTER_PREVIOUS_TRIGGER" == "undefined-key" ]] && \
    _ASTER_PREVIOUS_TRIGGER=set-mark-command
  typeset -g _ASTER_PREVIOUS_DOWN="${$(bindkey '^N')##* }"
  typeset -g _ASTER_PREVIOUS_UP="${$(bindkey '^K')##* }"
  [[ -z "$_ASTER_PREVIOUS_DOWN" || "$_ASTER_PREVIOUS_DOWN" == "undefined-key" ]] && \
    _ASTER_PREVIOUS_DOWN=down-line-or-history
  [[ -z "$_ASTER_PREVIOUS_UP" || "$_ASTER_PREVIOUS_UP" == "undefined-key" ]] && \
    _ASTER_PREVIOUS_UP=kill-line
  zle -A "$_ASTER_PREVIOUS_TRIGGER" _aster-native-trigger
  zle -A "$_ASTER_PREVIOUS_DOWN" _aster-native-down
  zle -A "$_ASTER_PREVIOUS_UP" _aster-native-up
  zle -A self-insert _aster-native-self-insert
  zle -A backward-delete-char _aster-native-backward-delete
  zle -A bracketed-paste _aster-native-bracketed-paste
  if [[ '__ASTER_COMPLETION_KEY__' != '^I' ]]; then
    typeset -g _ASTER_PREVIOUS_TAB="${$(bindkey '^I')##* }"
    [[ -z "$_ASTER_PREVIOUS_TAB" || "$_ASTER_PREVIOUS_TAB" == "undefined-key" ]] && \
      _ASTER_PREVIOUS_TAB=expand-or-complete
    zle -A "$_ASTER_PREVIOUS_TAB" _aster-native-tab
    zle -N aster-tab _aster_tab
    bindkey '^I' aster-tab
  fi
  zle -N aster-complete _aster_complete
  zle -N aster-menu-down _aster_menu_down
  zle -N aster-menu-up _aster_menu_up
  zle -N aster-menu-ready _aster_menu_request_ready
  zle -N aster-menu-apply _aster_menu_apply_result
  zle -N aster-menu-tick _aster_menu_tick
  zle -C aster-native-capture .complete-word _aster_native_capture_widget
  zle -N self-insert _aster_self_insert
  zle -N backward-delete-char _aster_backward_delete
  zle -N bracketed-paste _aster_bracketed_paste
  bindkey '__ASTER_COMPLETION_KEY__' aster-complete
  bindkey '^N' aster-menu-down
  bindkey '^K' aster-menu-up

  if [[ -n "${HISTFILE:-}" && -r "$HISTFILE" ]]; then
    command aster import-history --file "${HISTFILE:A}" >/dev/null 2>&1 &!
  fi
fi
"#
    .replace("__ASTER_COMPLETION_KEY__", &completion_key)
    .replace("__ASTER_COMPLETION_KEY_LABEL__", &completion_key_label)
    .replace("__ASTER_UI_MENU_WIDTH__", &menu_width)
    .replace("__ASTER_UI_MAX_VISIBLE__", &max_visible)
    .replace("__ASTER_COMPLETION_MAX_CANDIDATES__", &max_candidates)
    .replace(
        "__ASTER_UI_PROMPT_OFFSET__",
        &settings.ui.prompt_offset.to_string(),
    )
    .replace("__ASTER_UI_BORDER__", &settings.ui.border)
    .replace("__ASTER_UI_ACCENT__", &settings.ui.accent)
    .replace("__ASTER_UI_TEXT__", &settings.ui.text)
    .replace("__ASTER_UI_MUTED__", &settings.ui.muted)
    .replace("__ASTER_UI_GHOST__", &settings.ui.ghost)
    .replace(
        "__ASTER_UI_SELECTED_BACKGROUND__",
        &settings.ui.selected_background,
    )
    .replace("__ASTER_UI_SELECTED_TEXT__", &settings.ui.selected_text)
    .replace(
        "__ASTER_UI_SELECTED_SOURCE__",
        &settings.ui.selected_source,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_integration_uses_configured_completion_key() {
        let mut settings = Settings::default();
        let integration = zsh_integration(&settings).unwrap();
        assert!(integration.contains("bindkey '^@' aster-complete"));
        assert!(integration.contains("Ctrl-Space accept"));
        assert!(integration.contains("--format zsh-v2"));
        assert!(integration.contains("_ASTER_MENU_REFRESH_TICKS=5"));
        assert!(integration.contains("zle -C aster-native-capture"));
        assert!(integration.contains("descriptions+=(\"Zsh completion\")"));
        assert!(integration.contains("${(@)region_highlight:#*memo=aster*}"));
        assert!(integration.contains("_ASTER_FOREIGN_HIGHLIGHTS"));
        assert!(!integration.contains("aster-menu-enter"));
        assert!(!integration.contains("bindkey '^M'"));
        assert!(!integration.contains("__ASTER_COMPLETION_KEY__"));
        assert!(!integration.contains("__ASTER_UI_"));

        settings.completion.key = "ctrl-x".to_owned();
        settings.ui.selected_background = "#3a3228".to_owned();
        let integration = zsh_integration(&settings).unwrap();
        assert!(integration.contains("bindkey '^X' aster-complete"));
        assert!(integration.contains("bg=#3a3228"));
    }
}

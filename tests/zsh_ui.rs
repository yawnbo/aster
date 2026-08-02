use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tempfile::tempdir;

#[test]
fn popup_preserves_highlights_and_shell_bindings() {
    if !command_exists("tmux") || !command_exists("zsh") {
        eprintln!("skipping ZLE integration test because tmux or zsh is unavailable");
        return;
    }

    let aster = Path::new(env!("CARGO_BIN_EXE_aster"));
    let binary_directory = aster.parent().unwrap();
    let temporary = tempdir().unwrap();
    let state = temporary.path().join("state");
    let zdotdir = temporary.path().join("zsh");
    let helper_bin = temporary.path().join("bin");
    fs::create_dir(&state).unwrap();
    fs::create_dir(&zdotdir).unwrap();
    fs::create_dir(&helper_bin).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&zdotdir, fs::Permissions::from_mode(0o700)).unwrap();

    let config = temporary.path().join("aster.toml");
    let socket = state.join("aster.sock");
    let state_dump = temporary.path().join("zle-state");
    let preview_file = temporary.path().join("preview-target.txt");
    fs::write(&preview_file, "preview-first-line\npreview-second-line\n").unwrap();
    let fake_eza = helper_bin.join("eza");
    fs::write(
        &fake_eza,
        "#!/bin/sh\nprintf '\\033[31meza-alias-preview\\033[0m\\npreview-target.txt\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&fake_eza, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}:{}",
        helper_bin.display(),
        binary_directory.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let zshrc = format!(
        r#"export PATH={helper_bin}:$PATH
autoload -Uz add-zle-hook-widget compinit
compinit
_aster_test_highlight() {{
  region_highlight=( "${{(@)region_highlight:#*memo=foreign-test*}}" )
  [[ -n "$BUFFER" ]] && region_highlight+=("0 ${{#BUFFER}} fg=green memo=foreign-test")
}}
add-zle-hook-widget line-pre-redraw _aster_test_highlight
_aster_test_native() {{
  sleep 0.12
  if (( CURRENT > 2 )); then
    compadd next-value
  else
    compadd native/path/file native/second/file native/third/file
  fi
}}
compdef _aster_test_native aster-native-fixture
_aster_test_file() {{
  compadd {preview_file}
}}
compdef _aster_test_file aster-preview-fixture
alias ls=eza
eval "$({aster} init zsh)"
_aster_test_dump_state() {{
  print -r -- "$_ASTER_MENU_ACTIVE|${{#_ASTER_MENU_ACCEPTS}}|$_ASTER_MENU_BUFFER|$BUFFER|${{_ASTER_MENU_ACCEPTS[1]}}|$_ASTER_MENU_INDEX|${{_ASTER_MENU_DISPLAYS[$_ASTER_MENU_INDEX]}}|$_ASTER_FUZZY_ACTIVE|$_ASTER_FUZZY_BASE|$_ASTER_FUZZY_QUERY|$_ASTER_PREVIEW_FD|$_ASTER_PREVIEW_TICKS|$_ASTER_PREVIEW_PATH|${{(j:;:)_ASTER_PREVIEW_LINES}}" > {state_dump}
}}
zle -N _aster_test_dump_state
bindkey '^X^D' _aster_test_dump_state
bindkey -M aster-fuzzy '^X^D' _aster_test_dump_state
PROMPT='%# '
"#,
        aster = shell_quote(aster.to_str().unwrap()),
        helper_bin = shell_quote(helper_bin.to_str().unwrap()),
        preview_file = shell_quote(preview_file.to_str().unwrap()),
        state_dump = shell_quote(state_dump.to_str().unwrap())
    );
    fs::write(zdotdir.join(".zshrc"), zshrc).unwrap();
    let shell_environment = format!(
        "PATH={} ZDOTDIR={} ASTER_CONFIG={} ASTER_STATE_DIR={} ASTER_SOCKET={}",
        shell_quote(&path),
        shell_quote(zdotdir.to_str().unwrap()),
        shell_quote(config.to_str().unwrap()),
        shell_quote(state.to_str().unwrap()),
        shell_quote(socket.to_str().unwrap()),
    );

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let server = format!("aster-ui-{}-{unique}", std::process::id());
    let mut guard = ServerGuard {
        server: server.clone(),
        aster: aster.to_owned(),
        config: config.clone(),
        state: state.clone(),
        socket: socket.clone(),
        started: false,
    };

    let status = Command::new("tmux")
        .args([
            "-L",
            &server,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "test",
        ])
        .arg(format!("env {shell_environment} zsh -l"))
        .status()
        .unwrap();
    assert!(status.success(), "failed to start isolated tmux server");
    guard.started = true;

    thread::sleep(Duration::from_millis(300));
    let status = Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0", "aste"])
        .status()
        .unwrap();
    assert!(status.success(), "failed to type into isolated Zsh");

    let deadline = Instant::now() + Duration::from_secs(4);
    let mut capture = String::new();
    while Instant::now() < deadline {
        let output = Command::new("tmux")
            .args(["-L", &server, "capture-pane", "-p", "-e", "-t", "test:0.0"])
            .output()
            .unwrap();
        capture = String::from_utf8(output.stdout).unwrap();
        if capture.contains('╭') {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        capture.contains('╭'),
        "Aster popup did not appear:\n{capture}"
    );
    assert!(
        capture.contains("\u{1b}[32maste"),
        "existing syntax highlight was lost after popup rendering:\n{capture:?}"
    );

    let status = Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-Space"])
        .status()
        .unwrap();
    assert!(status.success(), "failed to accept the selected candidate");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut accepted_capture = String::new();
    while Instant::now() < deadline {
        accepted_capture = capture_pane(&server, true);
        if accepted_capture.contains("\u{1b}[32master") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        accepted_capture.contains("\u{1b}[32master"),
        "accepted command was not fully highlighted:\n{accepted_capture:?}"
    );

    let binding_file = temporary.path().join("enter-binding");
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-c"])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(200));
    let binding_command = format!(
        "{{ bindkey '^M'; bindkey '^I'; bindkey '^[[Z'; }} > {}",
        shell_quote(binding_file.to_str().unwrap())
    );
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0"])
        .arg(binding_command)
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "Enter"])
        .status()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !binding_file.exists() {
        thread::sleep(Duration::from_millis(50));
    }
    let binding = fs::read_to_string(&binding_file).unwrap_or_else(|error| {
        panic!(
            "Enter did not execute bindkey: {error}\n{}",
            capture_pane(&server, false)
        )
    });
    assert!(binding.contains(r#""^M" accept-line"#));
    assert!(binding.contains(r#""^I" aster-tab"#));
    assert!(binding.contains("aster-shift-tab"));

    Command::new("tmux")
        .args([
            "-L",
            &server,
            "send-keys",
            "-l",
            "-t",
            "test:0.0",
            "echo before",
        ])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0"])
        .arg("\u{1b}[200~ pasted\u{1b}[201~")
        .status()
        .unwrap();
    dump_zle_state(&server);
    let pasted_state = fs::read_to_string(&state_dump).unwrap();
    let pasted_fields: Vec<_> = pasted_state.trim_end().split('|').collect();
    assert_eq!(pasted_fields[3], "echo before pasted");
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-c"])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0"])
        .arg("aster-native-fixture na")
        .status()
        .unwrap();
    let immediate_capture = capture_pane(&server, false);
    assert!(
        !immediate_capture.contains("native/path/file"),
        "native completion ran synchronously instead of waiting for the debounce"
    );
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut native_capture = String::new();
    while Instant::now() < deadline {
        native_capture = capture_pane(&server, false);
        if native_capture.contains("aster-native-fixture native/path/file") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        native_capture.contains("aster-native-fixture native/path/file")
            && native_capture.contains("Zsh completion"),
        "debounced native Zsh candidate was absent:\n{native_capture}"
    );
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0", "t"])
        .status()
        .unwrap();
    let retained_capture = capture_pane(&server, false);
    assert!(
        retained_capture.contains('╭')
            && retained_capture.contains("aster-native-fixture native/path/file"),
        "matching candidates flashed off while the next request was pending:\n{retained_capture}"
    );
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-n", "C-n"])
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "BTab"])
        .status()
        .unwrap();
    dump_zle_state(&server);
    let shifted_state = fs::read_to_string(&state_dump).unwrap();
    assert!(
        shifted_state.contains("|2|aster-native-fixture native/second/file"),
        "Shift-Tab did not select the next candidate: {shifted_state:?}"
    );

    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0", "i"])
        .status()
        .unwrap();
    dump_zle_state(&server);
    let typed_state = fs::read_to_string(&state_dump).unwrap();
    assert!(
        typed_state.contains("|aster-native-fixture nati|aster-native-fixture nati|"),
        "ordinary typing did not update the buffer after selection: {typed_state:?}"
    );
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "BSpace"])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        native_capture = capture_pane(&server, false);
        if native_capture.contains("aster-native-fixture native/path/file") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        native_capture.contains("aster-native-fixture native/path/file"),
        "Backspace did not resume completion from the edited buffer:\n{native_capture}"
    );
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-n"])
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "Tab"])
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-x", "C-d"])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(20));
    let segment_capture = capture_pane(&server, false);
    let state_after_tab = fs::read_to_string(&state_dump).unwrap();
    assert!(
        state_after_tab
            .contains("|aster-native-fixture native/|aster-native-fixture native/|path/file")
            && state_after_tab.contains("|2|aster-native-fixture native/second/file"),
        "Tab did not accept only the next path segment; state was {state_after_tab:?}:\n{segment_capture}"
    );
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "Tab"])
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-x", "C-d"])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(20));
    let second_segment_capture = capture_pane(&server, false);
    let state_after_second_tab = fs::read_to_string(&state_dump).unwrap();
    assert!(
        state_after_second_tab.contains(
            "|aster-native-fixture native/second/|aster-native-fixture native/second/|file"
        ),
        "a repeated Tab did not accept the next path segment; state was {state_after_second_tab:?}:\n{second_segment_capture}"
    );
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "Tab"])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut follow_up_capture = String::new();
    while Instant::now() < deadline {
        follow_up_capture = capture_pane(&server, false);
        if follow_up_capture.contains("aster-native-fixture native/second/file next-value") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        follow_up_capture.contains("aster-native-fixture native/second/file next-value"),
        "native completion did not continue after accepting a complete match:\n{follow_up_capture}"
    );

    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-c"])
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0"])
        .arg("aster-native-fixture na")
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "Tab"])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(350));
    let tab_capture = capture_pane(&server, false);
    assert!(
        !tab_capture.contains("completion cannot be used recursively"),
        "ticker entered completion recursively during native Tab:\n{tab_capture}"
    );

    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-c"])
        .status()
        .unwrap();
    let source_command = format!(
        "zle -D _aster-native-space; source {}",
        shell_quote(zdotdir.join(".zshrc").to_str().unwrap())
    );
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0"])
        .arg(source_command)
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "Enter"])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    let rebound_file = temporary.path().join("reloaded-bindings");
    let rebound_command = format!(
        "{{ bindkey '^I'; bindkey '^[[Z'; bindkey '^@'; }} > {}",
        shell_quote(rebound_file.to_str().unwrap())
    );
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0"])
        .arg(rebound_command)
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "Enter"])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !rebound_file.exists() {
        thread::sleep(Duration::from_millis(20));
    }
    let rebound = fs::read_to_string(&rebound_file).unwrap();
    assert!(rebound.contains(r#""^I" aster-tab"#));
    assert!(rebound.contains("aster-shift-tab"));
    assert!(rebound.contains(r#""^@" aster-complete"#));

    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0", "aste"])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut reloaded_capture = String::new();
    while Instant::now() < deadline {
        reloaded_capture = capture_pane(&server, false);
        if reloaded_capture.contains('╭') {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        reloaded_capture.contains('╭'),
        "Aster did not restart its ticker after .zshrc was sourced:\n{reloaded_capture}"
    );

    let status = Command::new(aster)
        .args([
            "record",
            "--command",
            "echo fuzzy-history-target",
            "--cwd",
            temporary.path().to_str().unwrap(),
            "--exit-code",
            "0",
            "--session",
            "fuzzy-test",
        ])
        .env("ASTER_CONFIG", &config)
        .env("ASTER_STATE_DIR", &state)
        .env("ASTER_SOCKET", &socket)
        .status()
        .unwrap();
    assert!(status.success(), "failed to seed fuzzy history");
    let ls_history = format!("ls {}", temporary.path().display());
    let status = Command::new(aster)
        .args([
            "record",
            "--command",
            &ls_history,
            "--cwd",
            temporary.path().to_str().unwrap(),
            "--exit-code",
            "0",
            "--session",
            "preview-test",
        ])
        .env("ASTER_CONFIG", &config)
        .env("ASTER_STATE_DIR", &state)
        .env("ASTER_SOCKET", &socket)
        .status()
        .unwrap();
    assert!(status.success(), "failed to seed ls preview history");
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-c"])
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0", "  fzht"])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut fuzzy_capture = String::new();
    while Instant::now() < deadline {
        fuzzy_capture = capture_pane(&server, false);
        if fuzzy_capture.contains("echo fuzzy-history-target") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        fuzzy_capture.contains("echo fuzzy-history-target"),
        "inline fuzzy history result was absent:\n{fuzzy_capture}"
    );
    dump_zle_state(&server);
    let fuzzy_state = fs::read_to_string(&state_dump).unwrap();
    let fuzzy_fields: Vec<_> = fuzzy_state.trim_end().split('|').collect();
    assert_eq!(fuzzy_fields[3], " ");
    assert_eq!(fuzzy_fields[7], "1");
    assert_eq!(fuzzy_fields[8], " ");
    assert_eq!(fuzzy_fields[9], "fzht");

    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "Escape"])
        .status()
        .unwrap();
    dump_zle_state(&server);
    let escaped_state = fs::read_to_string(&state_dump).unwrap();
    let escaped_fields: Vec<_> = escaped_state.trim_end().split('|').collect();
    assert_eq!(escaped_fields[3], " ");
    assert_eq!(escaped_fields[7], "0");
    assert_eq!(escaped_fields[9], "");

    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0", " fzht"])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        fuzzy_capture = capture_pane(&server, false);
        if fuzzy_capture.contains("echo fuzzy-history-target") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "Tab"])
        .status()
        .unwrap();
    dump_zle_state(&server);
    let accepted_fuzzy_state = fs::read_to_string(&state_dump).unwrap();
    let accepted_fields: Vec<_> = accepted_fuzzy_state.trim_end().split('|').collect();
    assert_eq!(accepted_fields[3], "echo fuzzy-history-target");
    assert_eq!(accepted_fields[7], "0");

    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-c"])
        .status()
        .unwrap();
    Command::new("tmux")
        .args([
            "-L",
            &server,
            "resize-window",
            "-t",
            "test:0",
            "-x",
            "140",
            "-y",
            "40",
        ])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    Command::new("tmux")
        .args([
            "-L",
            &server,
            "send-keys",
            "-l",
            "-t",
            "test:0.0",
            "echo fuzzy",
        ])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut history_capture = String::new();
    while Instant::now() < deadline {
        history_capture = capture_pane(&server, false);
        if history_capture.contains("echo fuzzy-history-target") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(history_capture.contains("echo fuzzy-history-target"));
    assert!(
        !history_capture.contains("Preview:"),
        "history-only suggestions opened an unnecessary preview:\n{history_capture}"
    );
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-c"])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0", "ls "])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut command_preview_capture = String::new();
    while Instant::now() < deadline {
        command_preview_capture = capture_pane(&server, true);
        if command_preview_capture.contains("eza-alias-preview")
            && command_preview_capture.contains("\u{1b}[31meza-alias-preview")
            && command_preview_capture.contains("preview-target.txt")
            && command_preview_capture.contains("Preview:")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        command_preview_capture.contains("eza-alias-preview")
            && command_preview_capture.contains("\u{1b}[31meza-alias-preview")
            && command_preview_capture.contains("preview-target.txt")
            && command_preview_capture.contains("Preview:"),
        "safe eza-backed ls command preview was absent:\n{command_preview_capture}"
    );
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:0.0", "C-c"])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    let preview_prefix = preview_file.to_str().unwrap().strip_suffix("txt").unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0"])
        .arg(format!("aster-preview-fixture {preview_prefix}"))
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut preview_capture = String::new();
    while Instant::now() < deadline {
        preview_capture = capture_pane(&server, false);
        if preview_capture.contains("preview-first-line") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    dump_zle_state(&server);
    let preview_state = fs::read_to_string(&state_dump).unwrap();
    assert!(
        preview_capture.contains("preview-first-line") && preview_capture.contains("Preview:"),
        "lazy wide-terminal file preview was absent; state was {preview_state:?}:\n{preview_capture}"
    );
    Command::new("tmux")
        .args([
            "-L",
            &server,
            "resize-window",
            "-t",
            "test:0",
            "-x",
            "80",
            "-y",
            "24",
        ])
        .status()
        .unwrap();

    let ascii_shell = format!("env LC_ALL=C LANG=C {shell_environment} zsh -l");
    let status = Command::new("tmux")
        .args([
            "-L",
            &server,
            "new-window",
            "-d",
            "-t",
            "test",
            "-n",
            "ascii",
        ])
        .arg(ascii_shell)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "failed to start the non-UTF-8 Zsh fixture"
    );
    thread::sleep(Duration::from_millis(300));
    Command::new("tmux")
        .args([
            "-L",
            &server,
            "send-keys",
            "-l",
            "-t",
            "test:ascii.0",
            "aste",
        ])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut ascii_capture = String::new();
    while Instant::now() < deadline {
        ascii_capture = capture_target(&server, "test:ascii.0", false);
        if ascii_capture.contains("+-") && ascii_capture.contains("aster") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        ascii_capture.contains("+-") && ascii_capture.contains("| > "),
        "ASCII fallback menu was absent:\n{ascii_capture}"
    );
    assert!(
        !ascii_capture.contains("<e2>") && !ascii_capture.contains('╭'),
        "non-UTF-8 menu still contained Unicode rendering:\n{ascii_capture}"
    );
    assert_eq!(cursor_x(&server, "test:ascii.0"), 6);
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:ascii.0", "BSpace"])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    assert_eq!(cursor_x(&server, "test:ascii.0"), 5);
    Command::new("tmux")
        .args([
            "-L",
            &server,
            "send-keys",
            "-t",
            "test:ascii.0",
            "C-x",
            "C-d",
        ])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(20));
    let ascii_state = fs::read_to_string(&state_dump).unwrap();
    let ascii_fields: Vec<_> = ascii_state.trim_end().split('|').collect();
    assert_eq!(ascii_fields[3], "ast");
    Command::new("tmux")
        .args([
            "-L",
            &server,
            "resize-window",
            "-t",
            "test:ascii",
            "-x",
            "20",
            "-y",
            "15",
        ])
        .status()
        .unwrap();
    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-t", "test:ascii.0", "C-l"])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    let narrow_capture = capture_target(&server, "test:ascii.0", false);
    assert!(
        narrow_capture.contains("aster") && !narrow_capture.contains("+-"),
        "narrow terminal did not retain a compact suggestion:\n{narrow_capture}"
    );
    assert_eq!(cursor_x(&server, "test:ascii.0"), 5);
}

fn dump_zle_state(server: &str) {
    Command::new("tmux")
        .args(["-L", server, "send-keys", "-t", "test:0.0", "C-x", "C-d"])
        .status()
        .unwrap();
    thread::sleep(Duration::from_millis(20));
}

fn capture_pane(server: &str, include_escape_sequences: bool) -> String {
    capture_target(server, "test:0.0", include_escape_sequences)
}

fn cursor_x(server: &str, target: &str) -> usize {
    let output = Command::new("tmux")
        .args([
            "-L",
            server,
            "display-message",
            "-p",
            "-t",
            target,
            "#{cursor_x}",
        ])
        .output()
        .unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn capture_target(server: &str, target: &str, include_escape_sequences: bool) -> String {
    let mut command = Command::new("tmux");
    command.args(["-L", server, "capture-pane", "-p"]);
    if include_escape_sequences {
        command.arg("-e");
    }
    let output = command.args(["-t", target]).output().unwrap();
    String::from_utf8(output.stdout).unwrap()
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct ServerGuard {
    server: String,
    aster: std::path::PathBuf,
    config: std::path::PathBuf,
    state: std::path::PathBuf,
    socket: std::path::PathBuf,
    started: bool,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if self.started {
            let _ = Command::new("tmux")
                .args(["-L", &self.server, "kill-server"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = Command::new(&self.aster)
            .arg("stop")
            .env("ASTER_CONFIG", &self.config)
            .env("ASTER_STATE_DIR", &self.state)
            .env("ASTER_SOCKET", &self.socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

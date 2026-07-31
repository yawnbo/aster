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
    fs::create_dir(&state).unwrap();
    fs::create_dir(&zdotdir).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&zdotdir, fs::Permissions::from_mode(0o700)).unwrap();

    let config = temporary.path().join("aster.toml");
    let socket = state.join("aster.sock");
    let path = format!(
        "{}:{}",
        binary_directory.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let zshrc = format!(
        r#"autoload -Uz add-zle-hook-widget compinit
compinit
_aster_test_highlight() {{
  region_highlight=( "${{(@)region_highlight:#*memo=foreign-test*}}" )
  [[ -n "$BUFFER" ]] && region_highlight+=("0 ${{#BUFFER}} fg=green memo=foreign-test")
}}
add-zle-hook-widget line-pre-redraw _aster_test_highlight
_aster_test_native() {{
  compadd native-only
}}
compdef _aster_test_native aster-native-fixture
eval "$({aster} init zsh)"
PROMPT='%# '
"#,
        aster = shell_quote(aster.to_str().unwrap())
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
    let binding_command = format!(
        "bindkey '^M' > {}",
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
    let binding = fs::read_to_string(&binding_file)
        .unwrap_or_else(|error| panic!("Enter did not execute bindkey: {error}"));
    assert_eq!(binding.trim(), r#""^M" accept-line"#);

    Command::new("tmux")
        .args(["-L", &server, "send-keys", "-l", "-t", "test:0.0"])
        .arg("aster-native-fixture na")
        .status()
        .unwrap();
    let immediate_capture = capture_pane(&server, false);
    assert!(
        !immediate_capture.contains("native-only"),
        "native completion ran synchronously instead of waiting for the debounce"
    );
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut native_capture = String::new();
    while Instant::now() < deadline {
        native_capture = capture_pane(&server, false);
        if native_capture.contains("aster-native-fixture native-only") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        native_capture.contains("aster-native-fixture native-only")
            && native_capture.contains("Zsh completion"),
        "debounced native Zsh candidate was absent:\n{native_capture}"
    );

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
        ascii_capture.contains("+-") && ascii_capture.contains("| > $ aster"),
        "ASCII fallback menu was absent:\n{ascii_capture}"
    );
    assert!(
        !ascii_capture.contains("<e2>") && !ascii_capture.contains('╭'),
        "non-UTF-8 menu still contained Unicode rendering:\n{ascii_capture}"
    );
}

fn capture_pane(server: &str, include_escape_sequences: bool) -> String {
    capture_target(server, "test:0.0", include_escape_sequences)
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

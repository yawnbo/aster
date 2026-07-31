use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 1;
const CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024;
const OUTPUT_MAX_BYTES: u64 = 64 * 1024;
const QUEUE_CAPACITY: usize = 64;
const WORKER_COUNT: usize = 2;
const SUCCESS_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MISS_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAN_TIMEOUT: Duration = Duration::from_millis(1_500);
const HELP_TIMEOUT: Duration = Duration::from_millis(1_000);

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEntry {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandMatch {
    pub name: String,
    pub description: String,
    pub description_pending: bool,
}

#[derive(Debug)]
pub struct CommandCatalog {
    entries: Vec<CommandEntry>,
    jobs: HashMap<String, DescriptionJob>,
    state: Arc<Mutex<EnrichmentState>>,
    queue: Option<SyncSender<DescriptionJob>>,
}

#[derive(Debug, Default)]
struct EnrichmentState {
    descriptions: HashMap<String, String>,
    settled: HashSet<String>,
    pending: HashSet<String>,
}

#[derive(Debug, Clone)]
struct DescriptionJob {
    name: String,
    path: PathBuf,
    fingerprint: ExecutableFingerprint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ExecutableFingerprint {
    path: PathBuf,
    size: u64,
    device: u64,
    inode: u64,
    mode: u32,
    modified_secs: i64,
    modified_nanos: i64,
    changed_secs: i64,
    changed_nanos: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedDescription {
    fingerprint: ExecutableFingerprint,
    checked_at_secs: u64,
    description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DescriptionCache {
    version: u32,
    entries: BTreeMap<String, CachedDescription>,
}

impl Default for DescriptionCache {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

impl Default for CommandCatalog {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            jobs: HashMap::new(),
            state: Arc::new(Mutex::new(EnrichmentState::default())),
            queue: None,
        }
    }
}

impl CommandCatalog {
    pub fn discover(cache_file: PathBuf) -> Self {
        let cache = load_cache(&cache_file);
        let now = now_secs();
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        let mut jobs = HashMap::new();
        let mut state = EnrichmentState::default();

        for (name, description) in SHELL_BUILTINS {
            seen.insert((*name).to_owned());
            entries.push(CommandEntry {
                name: (*name).to_owned(),
                description: (*description).to_owned(),
            });
        }

        if let Some(path) = env::var_os("PATH") {
            for directory in env::split_paths(&path) {
                let Ok(children) = fs::read_dir(&directory) else {
                    continue;
                };
                for child in children.flatten() {
                    let Some(name) = child.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if !valid_name(&name) || seen.contains(&name) {
                        continue;
                    }
                    let path = child.path();
                    let Ok(metadata) = fs::metadata(&path) else {
                        continue;
                    };
                    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                        continue;
                    }
                    seen.insert(name.clone());
                    let authored = known_description(&name);
                    entries.push(CommandEntry {
                        description: authored
                            .map(str::to_owned)
                            .unwrap_or_else(|| fallback_description(&path)),
                        name: name.clone(),
                    });

                    if authored.is_none() && !name.starts_with('-') {
                        let fingerprint = fingerprint(&path, &metadata);
                        let job = DescriptionJob {
                            name: name.clone(),
                            path,
                            fingerprint,
                        };
                        if let Some(cached) = cache.entries.get(&name)
                            && cached.fingerprint == job.fingerprint
                            && cache_is_fresh(cached, now)
                        {
                            state.settled.insert(name.clone());
                            if let Some(description) = &cached.description {
                                state.descriptions.insert(name.clone(), description.clone());
                            }
                        }
                        jobs.insert(name, job);
                    }
                }
            }
        }

        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let state = Arc::new(Mutex::new(state));
        let queue = start_workers(Arc::clone(&state), cache_file, cache);
        Self {
            entries,
            jobs,
            state,
            queue,
        }
    }

    pub fn matching(&self, prefix: &str, limit: usize) -> Vec<CommandMatch> {
        let mut state = self.state.lock().expect("command state lock poisoned");
        self.entries
            .iter()
            .filter(|entry| entry.name.starts_with(prefix))
            .take(limit)
            .map(|entry| {
                let description = state
                    .descriptions
                    .get(&entry.name)
                    .cloned()
                    .unwrap_or_else(|| entry.description.clone());
                let mut description_pending = false;

                if let (Some(job), Some(queue)) = (self.jobs.get(&entry.name), self.queue.as_ref())
                    && !state.settled.contains(&entry.name)
                {
                    description_pending = true;
                    if state.pending.insert(entry.name.clone()) {
                        match queue.try_send(job.clone()) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                state.pending.remove(&entry.name);
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                state.pending.remove(&entry.name);
                                state.settled.insert(entry.name.clone());
                                description_pending = false;
                            }
                        }
                    }
                }

                CommandMatch {
                    name: entry.name.clone(),
                    description,
                    description_pending,
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub fn from_entries(entries: impl IntoIterator<Item = CommandEntry>) -> Self {
        let mut entries: Vec<_> = entries.into_iter().collect();
        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Self {
            entries,
            ..Self::default()
        }
    }
}

fn start_workers(
    state: Arc<Mutex<EnrichmentState>>,
    cache_file: PathBuf,
    cache: DescriptionCache,
) -> Option<SyncSender<DescriptionJob>> {
    let (sender, receiver) = sync_channel(QUEUE_CAPACITY);
    let receiver = Arc::new(Mutex::new(receiver));
    let cache = Arc::new(Mutex::new(cache));
    let mut started = 0;

    for index in 0..WORKER_COUNT {
        let state = Arc::clone(&state);
        let receiver = Arc::clone(&receiver);
        let cache = Arc::clone(&cache);
        let cache_file = cache_file.clone();
        let worker = thread::Builder::new()
            .name(format!("aster-description-{index}"))
            .spawn(move || description_worker(&receiver, &state, &cache_file, &cache));
        if worker.is_ok() {
            started += 1;
        }
    }

    (started > 0).then_some(sender)
}

fn description_worker(
    receiver: &Mutex<Receiver<DescriptionJob>>,
    state: &Mutex<EnrichmentState>,
    cache_file: &Path,
    cache: &Mutex<DescriptionCache>,
) {
    loop {
        let job = {
            let receiver = receiver.lock().expect("description queue lock poisoned");
            receiver.recv()
        };
        let Ok(job) = job else {
            return;
        };

        let unchanged_before = fingerprint_matches(&job);
        let description = unchanged_before
            .then(|| discover_description(&job, cache_file.parent().unwrap_or(Path::new("/tmp"))))
            .flatten();
        let cacheable = unchanged_before && fingerprint_matches(&job);
        {
            let mut state = state.lock().expect("command state lock poisoned");
            state.pending.remove(&job.name);
            state.settled.insert(job.name.clone());
            if cacheable && let Some(description) = &description {
                state
                    .descriptions
                    .insert(job.name.clone(), description.clone());
            }
        }

        if cacheable {
            let mut cache = cache.lock().expect("description cache lock poisoned");
            cache.entries.insert(
                job.name,
                CachedDescription {
                    fingerprint: job.fingerprint,
                    checked_at_secs: now_secs(),
                    description,
                },
            );
            let _ = save_cache(cache_file, &cache);
        }
    }
}

fn discover_description(job: &DescriptionJob, output_dir: &Path) -> Option<String> {
    man_description(&job.name, output_dir).or_else(|| help_description(job, output_dir))
}

fn man_description(name: &str, output_dir: &Path) -> Option<String> {
    let man = Path::new("/usr/bin/man");
    if !man.is_file() {
        return None;
    }
    let mut command = Command::new(man);
    command
        .arg(name)
        .env_clear()
        .env("HOME", "/nonexistent")
        .env("LC_ALL", "C")
        .env("MANPAGER", "cat")
        .env("PAGER", "cat");
    let output = run_bounded(command, output_dir, MAN_TIMEOUT)?;
    parse_man_description(name, &output)
}

#[cfg(target_os = "macos")]
fn help_description(job: &DescriptionJob, output_dir: &Path) -> Option<String> {
    let sandbox = Path::new("/usr/bin/sandbox-exec");
    if !sandbox.is_file() {
        return None;
    }
    let mut command = Command::new(sandbox);
    command
        .arg("-p")
        .arg(
            "(version 1) (deny default) (allow process-exec) (allow file-read*) \
             (allow sysctl-read) (allow mach-lookup)",
        )
        .arg(&job.path)
        .arg("--help")
        .env_clear()
        .env("HOME", "/nonexistent")
        .env("LC_ALL", "C")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .env("MANPAGER", "cat")
        .env("TERM", "dumb");
    let output = run_bounded(command, output_dir, HELP_TIMEOUT)?;
    parse_help_description(&job.name, &output)
}

#[cfg(not(target_os = "macos"))]
fn help_description(_job: &DescriptionJob, _output_dir: &Path) -> Option<String> {
    None
}

fn run_bounded(mut command: Command, output_dir: &Path, timeout: Duration) -> Option<String> {
    let (path, mut output) = temporary_output(output_dir)?;
    let stdout = output.try_clone().ok()?;
    let stderr = output.try_clone().ok()?;
    command
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .process_group(0);
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: OUTPUT_MAX_BYTES,
                rlim_max: OUTPUT_MAX_BYTES,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            let _ = fs::remove_file(path);
            return None;
        }
    };
    let _ = fs::remove_file(&path);
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();

    output.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    output.take(OUTPUT_MAX_BYTES).read_to_end(&mut bytes).ok()?;
    String::from_utf8(bytes).ok()
}

fn temporary_output(directory: &Path) -> Option<(PathBuf, File)> {
    for _ in 0..10 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            ".description-output-{}-{sequence}",
            std::process::id()
        ));
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path);
        match file {
            Ok(file) => return Some((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

fn parse_man_description(name: &str, output: &str) -> Option<String> {
    let mut name_section = false;
    for line in clean_lines(output) {
        if line == "NAME" {
            name_section = true;
            continue;
        }
        if name_section {
            name_section = false;
            if let Some(description) = line.strip_prefix(name)
                && description.chars().next().is_some_and(char::is_whitespace)
                && let Some(description) =
                    sanitize_description(description.trim_start().trim_start_matches('-'))
            {
                return Some(description);
            }
        }
        let Some((names, description)) = line.split_once(" - ") else {
            continue;
        };
        let exact = names.split(',').any(|candidate| {
            candidate
                .trim()
                .strip_prefix(name)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('('))
        });
        if exact && let Some(description) = sanitize_description(description) {
            return Some(description);
        }
    }
    None
}

fn parse_help_description(name: &str, output: &str) -> Option<String> {
    for line in clean_lines(output) {
        let lower = line.to_ascii_lowercase();
        let structural = [
            "usage",
            "options",
            "commands",
            "arguments",
            "available commands",
            "flags",
            "examples",
        ]
        .iter()
        .any(|heading| lower == *heading || lower.starts_with(&format!("{heading}:")));
        let command_usage =
            lower.starts_with(&format!("{name} [")) || lower.starts_with(&format!("{name} <"));
        let diagnostic = [
            "error:",
            "warning:",
            "failed",
            "couldn't",
            "unrecognized option",
            "unknown option",
        ]
        .iter()
        .any(|text| lower.contains(text));
        if structural || command_usage || diagnostic || line.starts_with(['-', '[']) {
            continue;
        }
        if let Some(description) = sanitize_description(&line)
            && description.split_whitespace().count() >= 2
        {
            return Some(description);
        }
    }
    None
}

fn clean_lines(output: &str) -> impl Iterator<Item = String> + '_ {
    output.lines().filter_map(|line| {
        let mut clean = String::with_capacity(line.len());
        let mut escape = 0;
        for character in line.chars() {
            if escape == 1 {
                escape = match character {
                    '[' => 2,
                    ']' => 3,
                    _ => 0,
                };
                continue;
            }
            if escape == 2 {
                if ('@'..='~').contains(&character) {
                    escape = 0;
                }
                continue;
            }
            if escape == 3 {
                continue;
            }
            if character == '\u{1b}' {
                escape = 1;
            } else if character == '\u{8}' {
                clean.pop();
            } else if character == '\t' {
                clean.push(' ');
            } else if !character.is_control() && !is_directional_format(character) {
                clean.push(character);
            }
        }
        let clean = clean.trim().to_owned();
        (!clean.is_empty()).then_some(clean)
    })
}

fn sanitize_description(description: &str) -> Option<String> {
    let mut clean = description.split_whitespace().collect::<Vec<_>>().join(" ");
    clean.retain(|character| !character.is_control() && !is_directional_format(character));
    if clean.is_empty() || !clean.chars().any(char::is_alphabetic) {
        return None;
    }
    if clean.chars().count() > 200 {
        clean = clean.chars().take(199).collect();
        clean.push('…');
    }
    Some(clean)
}

fn is_directional_format(character: char) -> bool {
    matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn load_cache(path: &Path) -> DescriptionCache {
    let Ok(metadata) = fs::metadata(path) else {
        return DescriptionCache::default();
    };
    if metadata.len() > CACHE_MAX_BYTES {
        return DescriptionCache::default();
    }
    let Ok(bytes) = fs::read(path) else {
        return DescriptionCache::default();
    };
    let Ok(cache) = serde_json::from_slice::<DescriptionCache>(&bytes) else {
        return DescriptionCache::default();
    };
    if cache.version != CACHE_VERSION {
        return DescriptionCache::default();
    }
    cache
}

fn save_cache(path: &Path, cache: &DescriptionCache) -> std::io::Result<()> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, cache)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn cache_is_fresh(cached: &CachedDescription, now: u64) -> bool {
    let ttl = if cached.description.is_some() {
        SUCCESS_TTL
    } else {
        MISS_TTL
    };
    now >= cached.checked_at_secs && now - cached.checked_at_secs <= ttl.as_secs()
}

fn fingerprint(path: &Path, metadata: &fs::Metadata) -> ExecutableFingerprint {
    ExecutableFingerprint {
        path: path.to_path_buf(),
        size: metadata.len(),
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        modified_secs: metadata.mtime(),
        modified_nanos: metadata.mtime_nsec(),
        changed_secs: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    }
}

fn fingerprint_matches(job: &DescriptionJob) -> bool {
    fs::metadata(&job.path)
        .map(|metadata| fingerprint(&job.path, &metadata) == job.fingerprint)
        .unwrap_or(false)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(char::is_control)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+'))
}

fn fallback_description(path: &Path) -> String {
    let path = path.to_string_lossy();
    if path.contains("/.cargo/bin/") {
        "Executable installed by Cargo".to_owned()
    } else if path.contains("/homebrew/") || path.contains("/Cellar/") {
        "Homebrew command".to_owned()
    } else if path.contains("/.local/bin/") || path.contains("/bin/") && path.contains("/Users/") {
        "User-installed command".to_owned()
    } else {
        "System command".to_owned()
    }
}

fn known_description(name: &str) -> Option<&'static str> {
    Some(match name {
        "ansible" => "Define and run automation tasks",
        "appwrite" => "Manage Appwrite projects and services",
        "arch" => "Print architecture type or run a universal binary",
        "asr" => "Apple Software Restore; copy volumes and disk images",
        "atlas" => "CLI tool to manage MongoDB Atlas",
        "aws" => "Official command line interface for Amazon Web Services",
        "aws-vault" => "Securely store and access AWS credentials",
        "bash" => "GNU Bourne Again shell",
        "brew" => "The missing package manager for macOS",
        "cargo" => "Rust package manager and build tool",
        "cmake" => "Configure, build, and test software projects",
        "code" => "Open Visual Studio Code",
        "curl" => "Transfer data from or to a server",
        "docker" => "Build and run applications in containers",
        "fd" => "Fast and user-friendly file finder",
        "fzf" => "Command-line fuzzy finder",
        "gh" => "GitHub command line interface",
        "git" => "Distributed version control system",
        "go" => "Build and manage Go source code",
        "iris" => "Interactive shell assistant",
        "jq" => "Process and transform JSON",
        "kubectl" => "Control Kubernetes clusters",
        "make" => "Maintain and build groups of programs",
        "node" => "Run JavaScript with Node.js",
        "npm" => "JavaScript package manager",
        "nvim" => "Edit text with Neovim",
        "pnpm" => "Fast, disk-efficient JavaScript package manager",
        "python" | "python3" => "Run the Python interpreter",
        "rg" => "Recursively search files with ripgrep",
        "rustc" => "Compile Rust source code",
        "ssh" => "OpenSSH remote login client",
        "tmux" => "Terminal multiplexer",
        "yarn" => "JavaScript package manager",
        "zsh" => "Z shell command interpreter",
        _ => return None,
    })
}

const SHELL_BUILTINS: &[(&str, &str)] = &[
    ("alias", "Define or display shell aliases"),
    ("autoload", "Mark shell functions for automatic loading"),
    ("bg", "Resume jobs in the background"),
    ("cd", "Change the current working directory"),
    ("command", "Execute a command without shell function lookup"),
    ("export", "Set environment variables for child processes"),
    ("fg", "Bring jobs into the foreground"),
    ("jobs", "Display active shell jobs"),
    ("setopt", "Enable Zsh options"),
    (
        "source",
        "Execute commands from a file in the current shell",
    ),
    ("typeset", "Declare shell variables and attributes"),
    ("unalias", "Remove shell alias definitions"),
    ("unset", "Remove shell variables or functions"),
    ("unsetopt", "Disable Zsh options"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn matches_sorted_prefixes() {
        let catalog = CommandCatalog::from_entries([
            CommandEntry {
                name: "atlas".to_owned(),
                description: "MongoDB Atlas".to_owned(),
            },
            CommandEntry {
                name: "arch".to_owned(),
                description: "Architecture".to_owned(),
            },
        ]);

        let names: Vec<_> = catalog
            .matching("a", 10)
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["arch", "atlas"]);
    }

    #[test]
    fn rejects_shell_metacharacters_in_names() {
        assert!(valid_name("aws-vault"));
        assert!(!valid_name("bad command"));
        assert!(!valid_name("bad;command"));
    }

    #[test]
    fn parses_exact_man_description() {
        let output = "assetutil(1) - process asset catalog.car files\n\
                      other(1) - unrelated\n";
        assert_eq!(
            parse_man_description("assetutil", output).as_deref(),
            Some("process asset catalog.car files")
        );
        assert_eq!(parse_man_description("asset", output), None);
    }

    #[test]
    fn parses_overstruck_man_name_section() {
        let output = "N\u{8}NA\u{8}AM\u{8}ME\u{8}E\n       a\u{8}as\u{8}s - assembler\n";
        assert_eq!(
            parse_man_description("as", output).as_deref(),
            Some("assembler")
        );
    }

    #[test]
    fn parses_man_name_section_without_separator() {
        let output = "NAME\n     assetutil process asset catalog files\n\nSYNOPSIS\n";
        assert_eq!(
            parse_man_description("assetutil", output).as_deref(),
            Some("process asset catalog files")
        );
    }

    #[test]
    fn parses_prose_from_help_output() {
        let output =
            "Usage: tool [OPTIONS]\n\nInspect a project without changing it.\n\nOptions:\n";
        assert_eq!(
            parse_help_description("tool", output).as_deref(),
            Some("Inspect a project without changing it.")
        );
    }

    #[test]
    fn rejects_help_diagnostics() {
        let output = "tool: error: couldn't create cache file\nUsage: tool [OPTIONS]\n";
        assert_eq!(parse_help_description("tool", output), None);
    }

    #[test]
    fn strips_terminal_controls_from_descriptions() {
        let output = "tool(1) - \u{1b}[31mred\u{1b}[0m\u{202e} text\n";
        assert_eq!(
            parse_man_description("tool", output).as_deref(),
            Some("red text")
        );
    }

    #[test]
    fn description_cache_round_trips() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("descriptions.json");
        let mut cache = DescriptionCache::default();
        cache.entries.insert(
            "tool".to_owned(),
            CachedDescription {
                fingerprint: ExecutableFingerprint {
                    path: PathBuf::from("/usr/bin/tool"),
                    size: 42,
                    device: 1,
                    inode: 2,
                    mode: 0o100755,
                    modified_secs: 3,
                    modified_nanos: 4,
                    changed_secs: 5,
                    changed_nanos: 6,
                },
                checked_at_secs: 7,
                description: Some("Inspect a tool".to_owned()),
            },
        );

        save_cache(&path, &cache).unwrap();
        let loaded = load_cache(&path);
        assert_eq!(
            loaded.entries["tool"].description.as_deref(),
            Some("Inspect a tool")
        );
        assert_eq!(loaded.entries["tool"].fingerprint.size, 42);
    }

    #[test]
    fn description_process_has_a_hard_timeout() {
        let directory = tempdir().unwrap();
        let mut command = Command::new("/bin/sleep");
        command.arg("2");
        let started = Instant::now();

        assert_eq!(
            run_bounded(command, directory.path(), Duration::from_millis(30)).as_deref(),
            Some("")
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

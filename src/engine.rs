use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use anyhow::{Result, bail};

use crate::commands::CommandCatalog;
use crate::config::{AcceptMode, Settings};
use crate::protocol::{Candidate, CandidateKind, CandidateSource, CompletionResponse};
use crate::store::Store;

const MAX_BUFFER_BYTES: usize = 64 * 1024;
const FUZZY_HISTORY_LIMIT: usize = 4096;
const MAX_DIRECTORY_ENTRIES: usize = 1024;

pub fn complete(
    store: &Store,
    commands: &CommandCatalog,
    buffer: &str,
    cursor_byte: usize,
    cwd: &str,
    requested_limit: Option<usize>,
    settings: &Settings,
) -> Result<CompletionResponse> {
    if buffer.len() > MAX_BUFFER_BYTES {
        bail!("completion buffer exceeds {MAX_BUFFER_BYTES} bytes");
    }
    if cursor_byte > buffer.len() || !buffer.is_char_boundary(cursor_byte) {
        bail!("cursor is not a valid UTF-8 byte offset");
    }

    // Mid-line replacement requires shell-aware token ranges. Abstain until that
    // parser exists rather than risking corruption of the user's command.
    if cursor_byte != buffer.len() || buffer.is_empty() {
        return Ok(CompletionResponse::empty(cursor_byte));
    }

    let limit = requested_limit
        .unwrap_or(settings.completion.max_candidates)
        .min(settings.completion.max_candidates);
    let history =
        store.history_candidates(buffer, cwd, limit, settings.history.successful_first)?;

    let mut candidates: Vec<_> = history
        .into_iter()
        .filter_map(|history| {
            let command = history.command;
            let suffix = command.strip_prefix(buffer)?;
            if suffix.is_empty() {
                return None;
            }
            let insert_text = suffix.to_owned();
            let accept_text = match settings.completion.accept {
                AcceptMode::Segment => next_segment(suffix),
                AcceptMode::Full => insert_text.clone(),
            };
            Some(Candidate {
                display: sanitize_display(&command),
                description: history_description(history.uses, history.same_cwd),
                description_pending: false,
                kind: CandidateKind::History,
                insert_text,
                accept_text,
                source: CandidateSource::History,
            })
        })
        .collect();

    if candidates.len() < limit && valid_command_prefix(buffer) {
        let remaining = limit - candidates.len();
        let command_candidates: Vec<_> = commands
            .matching(buffer, limit)
            .into_iter()
            .filter(|entry| {
                !candidates
                    .iter()
                    .any(|candidate| candidate.display == entry.name)
            })
            .take(remaining)
            .map(|entry| {
                let suffix = entry.name.strip_prefix(buffer).unwrap_or_default();
                let insertion = if suffix.is_empty() { " " } else { suffix };
                Candidate {
                    display: entry.name.clone(),
                    description: entry.description.clone(),
                    description_pending: entry.description_pending,
                    kind: CandidateKind::Command,
                    insert_text: insertion.to_owned(),
                    accept_text: insertion.to_owned(),
                    source: CandidateSource::Command,
                }
            })
            .collect();
        candidates.extend(command_candidates);
    }

    let mut enrichment_pending = false;
    if let Some((command, prefix)) = option_context(buffer) {
        let options = commands.matching_options(command, prefix, limit);
        enrichment_pending = options.pending;
        let option_candidates: Vec<_> = options
            .entries
            .into_iter()
            .filter_map(|option| {
                let suffix = option.spelling.strip_prefix(prefix)?;
                if suffix.is_empty() {
                    return None;
                }
                Some(Candidate {
                    display: format!("{buffer}{suffix}"),
                    description: option.description,
                    description_pending: false,
                    kind: CandidateKind::Option,
                    insert_text: suffix.to_owned(),
                    accept_text: suffix.to_owned(),
                    source: CandidateSource::Help,
                })
            })
            .collect();
        if !option_candidates.is_empty() {
            let option_slots = option_candidates.len().min((limit / 2).max(1));
            candidates.truncate(limit.saturating_sub(option_slots));
            candidates.extend(option_candidates.into_iter().take(option_slots));
        }
    }

    Ok(CompletionResponse {
        replace_start_byte: cursor_byte,
        replace_end_byte: cursor_byte,
        candidates,
        enrichment_pending,
    })
}

pub fn fuzzy(
    store: &Store,
    commands: &CommandCatalog,
    query: &str,
    cwd: &str,
    requested_limit: Option<usize>,
    settings: &Settings,
) -> Result<CompletionResponse> {
    if query.len() > MAX_BUFFER_BYTES {
        bail!("fuzzy query exceeds {MAX_BUFFER_BYTES} bytes");
    }
    let limit = requested_limit
        .unwrap_or(settings.completion.max_candidates)
        .min(settings.completion.max_candidates);
    let mut seen = HashSet::new();
    let mut pool = Vec::new();

    for history in
        store.history_inventory(cwd, FUZZY_HISTORY_LIMIT, settings.history.successful_first)?
    {
        if seen.insert(history.command.clone()) {
            pool.push(Candidate {
                display: sanitize_display(&history.command),
                description: history_description(history.uses, history.same_cwd),
                description_pending: false,
                kind: CandidateKind::History,
                insert_text: history.command.clone(),
                accept_text: history.command,
                source: CandidateSource::History,
            });
        }
    }
    for command in commands.inventory() {
        if seen.insert(command.name.clone()) {
            pool.push(Candidate {
                display: command.name.clone(),
                description: command.description,
                description_pending: false,
                kind: CandidateKind::Command,
                insert_text: command.name.clone(),
                accept_text: command.name,
                source: CandidateSource::Command,
            });
        }
    }

    let indexes = fzf_indexes(&pool, query, limit)?;
    let mut candidates = Vec::with_capacity(indexes.len());
    for index in indexes {
        let mut candidate = pool[index].clone();
        if candidate.source == CandidateSource::Command
            && let Some(command) = commands
                .matching(&candidate.display, 1)
                .into_iter()
                .find(|command| command.name == candidate.display)
        {
            candidate.description = command.description;
            candidate.description_pending = command.description_pending;
        }
        candidates.push(candidate);
    }
    Ok(CompletionResponse {
        replace_start_byte: 0,
        replace_end_byte: 0,
        candidates,
        enrichment_pending: false,
    })
}

pub fn filesystem_candidates(
    buffer: &str,
    cursor_byte: usize,
    cwd: &str,
    limit: usize,
) -> Result<Vec<Candidate>> {
    if limit == 0 || cursor_byte != buffer.len() || !buffer.is_char_boundary(cursor_byte) {
        return Ok(Vec::new());
    }
    let Some(argument_start) = current_argument_start(buffer) else {
        return Ok(Vec::new());
    };
    let Some(token) = unescape_path_token(&buffer[argument_start..]) else {
        return Ok(Vec::new());
    };
    if token.starts_with('-') {
        return Ok(Vec::new());
    }

    let (directory_text, name_prefix) = token
        .rfind('/')
        .map_or(("", token.as_str()), |slash| token.split_at(slash + 1));
    let directory = resolve_directory(directory_text, cwd);
    let Ok(children) = fs::read_dir(&directory) else {
        return Ok(Vec::new());
    };
    let show_hidden = name_prefix.starts_with('.');
    let mut matches = Vec::new();
    for child in children.take(MAX_DIRECTORY_ENTRIES).flatten() {
        let Some(name) = child.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.is_empty()
            || (!show_hidden && name.starts_with('.'))
            || !name.starts_with(name_prefix)
            || name.chars().any(char::is_control)
        {
            continue;
        }
        let Ok(file_type) = child.file_type() else {
            continue;
        };
        if !(file_type.is_dir() || file_type.is_file() || file_type.is_symlink()) {
            continue;
        }
        let is_directory = file_type.is_dir() || (file_type.is_symlink() && child.path().is_dir());
        let suffix = &name[name_prefix.len()..];
        let mut insert_text = escape_path_suffix(suffix);
        if token.is_empty() && name.starts_with('-') {
            insert_text.insert_str(0, "./");
        }
        if is_directory {
            insert_text.push('/');
        }
        if insert_text.is_empty() {
            continue;
        }
        matches.push((is_directory, name, insert_text));
    }
    matches.sort_unstable_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    Ok(matches
        .into_iter()
        .take(limit)
        .map(|(is_directory, _, insert_text)| Candidate {
            display: format!("{buffer}{insert_text}"),
            description: if is_directory { "Directory" } else { "File" }.to_owned(),
            description_pending: false,
            kind: if is_directory {
                CandidateKind::Directory
            } else {
                CandidateKind::File
            },
            accept_text: next_segment(&insert_text),
            insert_text,
            source: CandidateSource::Filesystem,
        })
        .collect())
}

pub fn merge_filesystem_candidates(
    response: &mut CompletionResponse,
    mut paths: Vec<Candidate>,
    limit: usize,
) {
    if paths.is_empty() || limit == 0 {
        return;
    }
    let history_count = response
        .candidates
        .iter()
        .take_while(|candidate| candidate.source == CandidateSource::History)
        .count();
    let path_slots = paths.len().min((limit / 2).max(1));
    if paths.len() > path_slots
        && let Some(first) = paths.first_mut()
    {
        first.description.push_str(" (more matches)");
    }
    let history_keep = history_count.min(limit.saturating_sub(path_slots));
    let mut original = std::mem::take(&mut response.candidates);
    let trailing = original.split_off(history_count);
    let history = original;
    let mut seen = HashSet::new();
    for path in paths {
        if response.candidates.len() >= path_slots {
            break;
        }
        if seen.insert(path.display.clone()) {
            response.candidates.push(path);
        }
    }
    for candidate in history.into_iter().take(history_keep) {
        if response.candidates.len() >= limit {
            break;
        }
        if seen.insert(candidate.display.clone()) {
            response.candidates.push(candidate);
        }
    }
    for candidate in trailing {
        if response.candidates.len() >= limit {
            break;
        }
        if seen.insert(candidate.display.clone()) {
            response.candidates.push(candidate);
        }
    }
}

fn resolve_directory(directory_text: &str, cwd: &str) -> PathBuf {
    if directory_text == "~/" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(cwd));
    }
    if let Some(relative) = directory_text.strip_prefix("~/") {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(cwd))
            .join(relative);
    }
    let directory = Path::new(directory_text);
    if directory.is_absolute() {
        directory.to_owned()
    } else {
        Path::new(cwd).join(directory)
    }
}

fn unescape_path_token(value: &str) -> Option<String> {
    let mut unescaped = String::new();
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            unescaped.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character.is_control() || "'\";&|><$`(){}[]!*?".contains(character) {
            return None;
        } else {
            unescaped.push(character);
        }
    }
    (!escaped).then_some(unescaped)
}

fn current_argument_start(buffer: &str) -> Option<usize> {
    let mut start = None;
    let mut escaped = false;
    for (index, character) in buffer.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character.is_whitespace() {
            start = Some(index + character.len_utf8());
        }
    }
    start
}

fn escape_path_suffix(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        if character.is_ascii()
            && !(character.is_ascii_alphanumeric() || "_-+.@%,".contains(character))
        {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn option_context(buffer: &str) -> Option<(&str, &str)> {
    if buffer
        .chars()
        .any(|character| character.is_control() || "'\"\\;&|><$`(){}[]!*?".contains(character))
    {
        return None;
    }
    let argument_start = buffer.rfind(char::is_whitespace)? + 1;
    let prefix = &buffer[argument_start..];
    if !prefix.starts_with('-') {
        return None;
    }
    let mut words = buffer[..argument_start].split_ascii_whitespace();
    let command = words.next()?;
    if !valid_command_prefix(command) || words.any(|word| !word.starts_with('-') || word == "--") {
        return None;
    }
    Some((command, prefix))
}

fn fzf_indexes(candidates: &[Candidate], query: &str, limit: usize) -> Result<Vec<usize>> {
    if candidates.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut input = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        write!(input, "{index}\t{}\0", candidate.display)?;
    }

    let mut child = Command::new("fzf")
        .args([
            "--read0",
            "--print0",
            "--no-multi",
            "--delimiter=\\t",
            "--nth=2..",
            "--tiebreak=index",
            "--filter",
            query,
        ])
        .env_remove("FZF_DEFAULT_OPTS")
        .env_remove("FZF_DEFAULT_OPTS_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("fzf stdin is piped");
    let writer = thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output()?;
    writer.join().expect("fzf input writer panicked")?;
    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    if !output.status.success() {
        bail!("fzf fuzzy filter failed with {}", output.status);
    }

    let mut indexes = Vec::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() || indexes.len() >= limit {
            continue;
        }
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let index = std::str::from_utf8(&record[..tab])?.parse::<usize>()?;
        if index < candidates.len() {
            indexes.push(index);
        }
    }
    Ok(indexes)
}

fn valid_command_prefix(buffer: &str) -> bool {
    !buffer.is_empty()
        && !buffer.starts_with('.')
        && buffer
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'+'))
}

fn history_description(uses: usize, same_cwd: bool) -> String {
    match (uses, same_cwd) {
        (1, true) => "used here".to_owned(),
        (1, false) => "used once".to_owned(),
        (uses, true) => format!("used {uses}x, here"),
        (uses, false) => format!("used {uses}x"),
    }
}

fn sanitize_display(value: &str) -> String {
    let mut display = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            display.extend(character.escape_default());
        } else {
            display.push(character);
        }
    }
    display
}

pub fn next_segment(suffix: &str) -> String {
    let mut saw_non_whitespace = false;
    for (index, character) in suffix.char_indices() {
        let end = index + character.len_utf8();
        if character.is_whitespace() {
            if saw_non_whitespace {
                return suffix[..end].to_owned();
            }
            continue;
        }
        saw_non_whitespace = true;
        if matches!(character, '/' | '=' | ':' | ',') {
            return suffix[..end].to_owned();
        }
    }
    suffix.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{CommandCatalog, CommandEntry, OptionMatch};
    use crate::config::Settings;
    use crate::store::Store;
    use tempfile::tempdir;

    #[test]
    fn accepts_one_path_segment() {
        assert_eq!(next_segment("ev/gitrepos/aster"), "ev/");
    }

    #[test]
    fn accepts_one_shell_word() {
        assert_eq!(next_segment(" checkout feature/topic"), " checkout ");
    }

    #[test]
    fn accepts_remaining_text_without_boundary() {
        assert_eq!(next_segment("status"), "status");
    }

    #[test]
    fn escapes_control_characters_in_display_text() {
        assert_eq!(sanitize_display("echo\t\u{1b}"), "echo\\t\\u{1b}");
    }

    #[test]
    fn completes_filesystem_entries_at_argument_positions() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("alpha-dir")).unwrap();
        fs::write(directory.path().join("alpha-file"), "file").unwrap();
        fs::write(directory.path().join("alpha space"), "file").unwrap();
        fs::write(directory.path().join(".hidden"), "file").unwrap();
        fs::write(directory.path().join("-rf"), "file").unwrap();
        fs::write(directory.path().join("=command"), "file").unwrap();
        fs::create_dir(directory.path().join("space dir")).unwrap();
        fs::write(directory.path().join("space dir/child"), "file").unwrap();

        let buffer = "scp -r alpha";
        let candidates =
            filesystem_candidates(buffer, buffer.len(), directory.path().to_str().unwrap(), 10)
                .unwrap();
        assert_eq!(candidates[0].display, "scp -r alpha-dir/");
        assert_eq!(candidates[0].kind, CandidateKind::Directory);
        assert!(candidates.iter().any(|candidate| {
            candidate.display == "scp -r alpha-file" && candidate.kind == CandidateKind::File
        }));
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.display == "scp -r alpha\\ space")
        );
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.display.contains(".hidden"))
        );

        let nested = "scp -r space\\ dir/ch";
        let candidates =
            filesystem_candidates(nested, nested.len(), directory.path().to_str().unwrap(), 10)
                .unwrap();
        assert_eq!(candidates[0].display, "scp -r space\\ dir/child");

        let blank = "scp -r ";
        let candidates =
            filesystem_candidates(blank, blank.len(), directory.path().to_str().unwrap(), 20)
                .unwrap();
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.display == "scp -r ./-rf")
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.display == "scp -r \\=command")
        );
    }

    #[test]
    fn lists_paths_after_an_empty_argument_and_hides_them_in_command_position() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("visible"), "file").unwrap();

        let buffer = "command ";
        let candidates =
            filesystem_candidates(buffer, buffer.len(), directory.path().to_str().unwrap(), 10)
                .unwrap();
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.display == "command visible")
        );
        assert!(
            filesystem_candidates("com", 3, directory.path().to_str().unwrap(), 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn filesystem_candidates_reserve_capacity_after_history() {
        let mut response = CompletionResponse {
            replace_start_byte: 5,
            replace_end_byte: 5,
            candidates: (0..4)
                .map(|index| Candidate {
                    display: format!("cmd history-{index}"),
                    description: String::new(),
                    description_pending: false,
                    kind: CandidateKind::History,
                    insert_text: index.to_string(),
                    accept_text: index.to_string(),
                    source: CandidateSource::History,
                })
                .collect(),
            enrichment_pending: false,
        };
        let paths = (0..2)
            .map(|index| Candidate {
                display: format!("cmd path-{index}"),
                description: "File".to_owned(),
                description_pending: false,
                kind: CandidateKind::File,
                insert_text: index.to_string(),
                accept_text: index.to_string(),
                source: CandidateSource::Filesystem,
            })
            .collect();
        merge_filesystem_candidates(&mut response, paths, 4);
        assert_eq!(response.candidates.len(), 4);
        assert_eq!(response.candidates[0].source, CandidateSource::Filesystem);
        assert_eq!(
            response
                .candidates
                .iter()
                .filter(|candidate| candidate.source == CandidateSource::Filesystem)
                .count(),
            2
        );

        let mut response = CompletionResponse::empty(0);
        let paths = (0..2)
            .map(|index| Candidate {
                display: format!("cmd path-{index}"),
                description: "File".to_owned(),
                description_pending: false,
                kind: CandidateKind::File,
                insert_text: index.to_string(),
                accept_text: index.to_string(),
                source: CandidateSource::Filesystem,
            })
            .collect();
        merge_filesystem_candidates(&mut response, paths, 1);
        assert_eq!(response.candidates[0].description, "File (more matches)");
    }

    #[test]
    fn recognizes_only_safe_root_option_contexts() {
        assert_eq!(option_context("git --ver"), Some(("git", "--ver")));
        assert_eq!(option_context("git status --short"), None);
        assert_eq!(
            option_context("git --quiet --short"),
            Some(("git", "--short"))
        );
        assert_eq!(option_context("git -- --literal"), None);
        assert_eq!(option_context("git \"--ver"), None);
        assert_eq!(option_context("--ver"), None);
    }

    #[test]
    fn completes_cached_root_command_options() {
        let store = Store::in_memory().unwrap();
        let commands = CommandCatalog::from_options(
            "tool",
            vec![
                OptionMatch {
                    spelling: "--verbose".to_owned(),
                    description: "Show verbose output".to_owned(),
                },
                OptionMatch {
                    spelling: "--version".to_owned(),
                    description: "Print version".to_owned(),
                },
            ],
        );
        let completion = complete(
            &store,
            &commands,
            "tool --ver",
            "tool --ver".len(),
            "/repo",
            None,
            &Settings::default(),
        )
        .unwrap();
        assert_eq!(completion.candidates.len(), 2);
        assert_eq!(completion.candidates[0].display, "tool --verbose");
        assert_eq!(completion.candidates[0].kind, CandidateKind::Option);
        assert_eq!(completion.candidates[0].source, CandidateSource::Help);
        assert_eq!(completion.candidates[1].display, "tool --version");
    }

    #[test]
    fn describes_history_usage_and_directory() {
        assert_eq!(history_description(1, true), "used here");
        assert_eq!(history_description(3, true), "used 3x, here");
        assert_eq!(history_description(2, false), "used 2x");
    }

    #[test]
    fn completes_history_with_a_single_segment() {
        let store = Store::in_memory().unwrap();
        let mut settings = Settings::default();
        settings.completion.accept = AcceptMode::Segment;
        store
            .record("cd ~/dev/gitrepos/aster", "/repo", 0, 100, "test", true)
            .unwrap();

        let completion = complete(
            &store,
            &CommandCatalog::default(),
            "cd ~/d",
            "cd ~/d".len(),
            "/repo",
            None,
            &settings,
        )
        .unwrap();

        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(completion.candidates[0].insert_text, "ev/gitrepos/aster");
        assert_eq!(completion.candidates[0].accept_text, "ev/");
        assert_eq!(completion.candidates[0].description, "used here");

        let completion = complete(
            &store,
            &CommandCatalog::default(),
            "cd ~/d",
            "cd ~/d".len(),
            "/repo",
            None,
            &Settings::default(),
        )
        .unwrap();
        assert_eq!(completion.candidates[0].accept_text, "ev/gitrepos/aster");
    }

    #[test]
    fn fuzzy_searches_history_without_a_prefix() {
        if Command::new("fzf").arg("--version").output().is_err() {
            return;
        }
        let store = Store::in_memory().unwrap();
        store
            .record("cargo test --all", "/repo", 0, 100, "test", true)
            .unwrap();
        let completion = fuzzy(
            &store,
            &CommandCatalog::default(),
            "cgt",
            "/repo",
            None,
            &Settings::default(),
        )
        .unwrap();
        assert_eq!(completion.candidates[0].display, "cargo test --all");
    }

    #[test]
    fn abstains_from_mid_line_completion() {
        let store = Store::in_memory().unwrap();
        let completion = complete(
            &store,
            &CommandCatalog::default(),
            "git status",
            3,
            "/repo",
            None,
            &Settings::default(),
        )
        .unwrap();
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn discovers_commands_when_history_abstains() {
        let store = Store::in_memory().unwrap();
        let commands = CommandCatalog::from_entries([CommandEntry {
            name: "atlas".to_owned(),
            description: "CLI tool to manage MongoDB Atlas".to_owned(),
        }]);

        let completion = complete(
            &store,
            &commands,
            "atl",
            3,
            "/repo",
            None,
            &Settings::default(),
        )
        .unwrap();

        assert_eq!(completion.candidates[0].display, "atlas");
        assert_eq!(completion.candidates[0].accept_text, "as");
        assert_eq!(completion.candidates[0].kind, CandidateKind::Command);
    }

    #[test]
    fn history_precedes_command_inventory() {
        let store = Store::in_memory().unwrap();
        store
            .record("git status", "/repo", 0, 100, "test", true)
            .unwrap();
        let commands = CommandCatalog::from_entries([CommandEntry {
            name: "git-town".to_owned(),
            description: "Git workflow automation".to_owned(),
        }]);

        let completion = complete(
            &store,
            &commands,
            "git",
            3,
            "/repo",
            None,
            &Settings::default(),
        )
        .unwrap();

        assert_eq!(completion.candidates[0].source, CandidateSource::History);
        assert_eq!(completion.candidates[1].source, CandidateSource::Command);
    }

    #[test]
    fn history_deduplicates_command_inventory() {
        let store = Store::in_memory().unwrap();
        store
            .record("atlas", "/repo", 0, 100, "test", true)
            .unwrap();
        let commands = CommandCatalog::from_entries([
            CommandEntry {
                name: "atlas".to_owned(),
                description: "CLI tool to manage MongoDB Atlas".to_owned(),
            },
            CommandEntry {
                name: "atlantis".to_owned(),
                description: "Terraform pull request automation".to_owned(),
            },
        ]);

        let completion = complete(
            &store,
            &commands,
            "atl",
            3,
            "/repo",
            None,
            &Settings::default(),
        )
        .unwrap();

        assert_eq!(completion.candidates.len(), 2);
        assert_eq!(completion.candidates[0].display, "atlas");
        assert_eq!(completion.candidates[0].source, CandidateSource::History);
        assert_eq!(completion.candidates[1].display, "atlantis");
    }
}

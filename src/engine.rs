use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;

use anyhow::{Result, bail};

use crate::commands::CommandCatalog;
use crate::config::{AcceptMode, Settings};
use crate::protocol::{Candidate, CandidateKind, CandidateSource, CompletionResponse};
use crate::store::Store;

const MAX_BUFFER_BYTES: usize = 64 * 1024;
const FUZZY_HISTORY_LIMIT: usize = 4096;

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

    Ok(CompletionResponse {
        replace_start_byte: cursor_byte,
        replace_end_byte: cursor_byte,
        candidates,
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
    })
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
    use crate::commands::{CommandCatalog, CommandEntry};
    use crate::config::Settings;
    use crate::store::Store;

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

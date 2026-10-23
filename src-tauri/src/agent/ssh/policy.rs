// Policy/limits for ssh_exec: the read-only command allowlist that decides
// whether a call skips confirmation, and the destructive-command labeling
// that only enriches the confirmation text (never blocks by itself).
//
// ---- read-only command allowlist (pure, decides the ssh_exec policy) ----

// Commands that only read remote state. Multi-word entries need both the
// program and its read-only subcommand/arguments matched exactly.
const READ_ONLY_COMMANDS: [&str; 23] = [
    "ls", "cat", "head", "tail", "grep", "find", "ps", "top", "df", "du", "free", "uptime",
    "whoami", "id", "pwd", "uname", "hostname", "date", "ss", "netstat", "wc", "stat", "file",
];
const READ_ONLY_COMMAND_PAIRS: [&str; 6] = [
    "git status",
    "git log",
    "git diff",
    "docker ps",
    "systemctl status",
    "ip addr",
];

// A command is clearly read-only only when its first token (or the first two
// for the multi-word entries) is on the allowlist AND it carries no shell
// metacharacters, redirections, substitutions or separators that could hide
// a second command: ; | > < & (covering && and ||), backticks, "$(", and
// newlines. Everything else stays on the confirmation side.
pub(super) fn is_read_only_command(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() || command.contains([';', '|', '>', '<', '&', '`', '\n', '\r']) {
        return false;
    }
    if command.contains("$(") {
        return false;
    }
    let mut words = command.split_whitespace();
    let first = words.next().unwrap_or("");
    if READ_ONLY_COMMANDS.contains(&first) {
        return true;
    }
    let second = words.next().unwrap_or("");
    READ_ONLY_COMMAND_PAIRS.contains(&format!("{first} {second}").as_str())
}

// ---- destructive-command labeling (pure, never blocks) ----

// The confirmation is the guard; this only explains WHY a command looks
// dangerous in the confirmation queue. It errs on the side of flagging: a
// false positive costs nothing extra (non-read-only commands confirm
// anyway).
const WRAPPERS: [&str; 5] = ["sudo", "env", "xargs", "timeout", "nohup"];

fn basename_word(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn is_duration(token: &str) -> bool {
    let digits = token.trim_end_matches(['s', 'm', 'h', 'd']);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn has_short_flag(args: &[String], letter: char) -> bool {
    args.iter()
        .any(|a| a.starts_with('-') && !a.starts_with("--") && a[1..].contains(letter))
}

// Quotes and backslashes are blanked so `sh -c 'rm -rf /'` tokenises like
// the inner command; segments split on ; && || | and newlines; leading
// wrapper words (and their options, durations and assignments) are skipped
// to find the effective command word of each segment.
fn effective_commands(command: &str) -> Vec<(String, Vec<String>)> {
    let cleaned: String = command
        .chars()
        .map(|c| if matches!(c, '\'' | '"' | '\\') { ' ' } else { c })
        .collect();
    let mut out = Vec::new();
    for segment in cleaned.split([';', '|', '&', '\n']) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        let mut i = 0;
        while i < tokens.len() {
            let token = tokens[i];
            if is_assignment(token) {
                i += 1;
                continue;
            }
            let word = basename_word(token);
            if WRAPPERS.contains(&word) {
                i += 1;
                while i < tokens.len() {
                    let next = tokens[i];
                    if next.starts_with('-') || is_duration(next) || is_assignment(next) {
                        i += 1;
                    } else {
                        break;
                    }
                }
                continue;
            }
            out.push((
                word.to_string(),
                tokens[i + 1..].iter().map(|t| t.to_string()).collect(),
            ));
            break;
        }
    }
    out
}

// `:(){` with optional whitespace between the parts (the fork bomb head).
fn contains_fork_bomb(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    'outer: for i in 0..chars.len() {
        if chars[i] != ':' {
            continue;
        }
        let mut j = i + 1;
        for want in ['(', ')', '{'] {
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j >= chars.len() || chars[j] != want {
                continue 'outer;
            }
            j += 1;
        }
        return true;
    }
    false
}

// Any redirect into /dev (device writes; also flags harmless 2>/dev/null —
// this is labeling for the confirmation text, not a block).
fn writes_to_dev(text: &str) -> bool {
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'>' {
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
            j += 1;
        }
        if bytes[j..].starts_with(b"/dev/") {
            return true;
        }
    }
    false
}

// Human-readable Russian reasons for every matched destructive pattern;
// empty when the command looks safe. Never blocks execution by itself.
pub(crate) fn destructive_reasons(command: &str) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::new();
    let mut add = |reason: &str| {
        if !reasons.iter().any(|r| r == reason) {
            reasons.push(reason.to_string());
        }
    };
    for (word, args) in effective_commands(command) {
        let word = word.as_str();
        if word == "rm"
            && (has_short_flag(&args, 'r')
                || has_short_flag(&args, 'R')
                || args.iter().any(|a| a == "--recursive"))
        {
            add("рекурсивное удаление (rm -r)");
        }
        if word == "dd" {
            add("низкоуровневая запись на диск (dd)");
        }
        if word.starts_with("mkfs") {
            add("форматирование файловой системы (mkfs)");
        }
        if matches!(word, "shutdown" | "reboot" | "poweroff" | "halt") {
            add("выключение или перезагрузка машины");
        }
        if word == "git"
            && args.iter().any(|a| a == "reset")
            && args.iter().any(|a| a == "--hard")
        {
            add("жёсткий сброс рабочего дерева git (git reset --hard)");
        }
        if word == "chmod"
            && (has_short_flag(&args, 'R') || args.iter().any(|a| a == "--recursive"))
        {
            add("рекурсивная смена прав (chmod -R)");
        }
        if matches!(word, "killall" | "pkill") {
            add("массовое завершение процессов (killall/pkill)");
        }
    }
    if contains_fork_bomb(command) {
        add("fork-бомба");
    }
    if writes_to_dev(command) {
        add("запись напрямую в /dev");
    }
    reasons
}

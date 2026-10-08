use super::CommandEvent;
use regex::Regex;
use std::{path::Path, sync::LazyLock};

// These constant expressions are tested below. Neither errors nor raw logs are
// included in diagnostics; redaction is applied to every exported string field.
static HOME_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:/(?:home|Users)/|[A-Z]:[\\/]Users[\\/])([^/\\\s'"<>]+)"#)
        .expect("constant home path expression")
});
// Locate sensitive value prefixes, then scan entire shell words rather than
// letting a regex stop at an escaped quote or a concatenated quoted fragment.
static SECRET_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?ix)(?:[\w.-]*(?:password|passwd|token|secret|credential|api[_-]?key|access[_-]?key|private[_-]?key)[\w.-]*\s*(?:=|:)\s*|--?(?:password|passwd|token|secret|api[_-]?key|access[_-]?key)\s+|authorization\s*[:=]\s*(?:bearer\s+|basic\s+)?|(?:^|[\s;|&])(?:--(?:proxy-)?user(?:=|\s+)|-[uU]\s*))")
        .expect("constant sensitive value prefix expression")
});
static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(\w+://)[^\s/@'"<>]+@"#).expect("constant URL expression"));
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:gh[pousr]_[A-Za-z0-9_]+|github_pat_[A-Za-z0-9_]+|sk-[A-Za-z0-9_-]{8,}|AKIA[A-Z0-9]{16}|eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+)\b").expect("constant token expression")
});

pub fn redact_events(
    events: &mut [CommandEvent],
    workspace: Option<&Path>,
    extra_patterns: &[String],
) {
    let mut replacements: Vec<(String, String)> = Vec::new();
    let mut users = Vec::new();
    for name in ["USER", "USERNAME"] {
        if let Ok(user) = std::env::var(name)
            && !user.is_empty()
        {
            users.push(user);
        }
    }
    for name in ["HOME", "USERPROFILE"] {
        if let Ok(home) = std::env::var(name)
            && home.len() > 1
        {
            replacements.push((home, "<HOME>".to_owned()));
        }
    }
    // Discover foreign usernames too: archived logs need not belong to this user.
    for event in events.iter() {
        for text in [
            &event.command,
            event.cwd.as_deref().unwrap_or(""),
            event.project.as_deref().unwrap_or(""),
        ] {
            users.extend(
                HOME_PATH
                    .captures_iter(text)
                    .filter_map(|c| c.get(1).map(|m| m.as_str().to_owned())),
            );
        }
        for path in [event.project.as_deref(), event.cwd.as_deref()]
            .into_iter()
            .flatten()
        {
            if is_path(path) && path.len() > 1 {
                replacements.push((path.to_owned(), "<WORKSPACE>".to_owned()));
            }
        }
    }
    if let Some(workspace) = workspace {
        replacements.push((
            workspace.to_string_lossy().into_owned(),
            "<WORKSPACE>".to_owned(),
        ));
    }
    for pattern in extra_patterns.iter().filter(|s| !s.is_empty()) {
        replacements.push((pattern.clone(), "<REDACTED>".to_owned()));
    }
    replacements.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.cmp(b)));
    replacements.dedup();
    users.sort();
    users.dedup();
    let usernames = users
        .iter()
        .map(|user| {
            Regex::new(&format!(r"\b{}\b", regex::escape(user)))
                .expect("escaped username expression")
        })
        .collect::<Vec<_>>();
    for event in events {
        for text in [&mut event.ide, &mut event.tool, &mut event.command] {
            redact(text, &replacements, &usernames);
        }
        for text in [
            &mut event.timestamp,
            &mut event.agent,
            &mut event.project,
            &mut event.session,
            &mut event.cwd,
            &mut event.call_id,
        ]
        .into_iter()
        .flatten()
        {
            redact(text, &replacements, &usernames);
        }
        if let Some(maven) = &mut event.maven {
            redact(&mut maven.executable, &replacements, &usernames);
            for text in maven
                .lifecycle_goals
                .iter_mut()
                .chain(maven.plugin_goals.iter_mut())
                .chain(maven.modules.iter_mut())
                .chain(maven.tests.iter_mut())
                .chain(maven.profiles.iter_mut())
                .chain(maven.resume_from.iter_mut())
                .chain(maven.pom_file.iter_mut())
            {
                redact(text, &replacements, &usernames);
            }
            maven.properties = std::mem::take(&mut maven.properties)
                .into_iter()
                .map(|(mut key, mut value)| {
                    if [
                        "password",
                        "passwd",
                        "token",
                        "secret",
                        "credential",
                        "authorization",
                        "apikey",
                        "accesskey",
                        "privatekey",
                    ]
                    .iter()
                    .any(|s| key.to_ascii_lowercase().replace(['_', '-'], "").contains(s))
                    {
                        value = "<REDACTED>".to_owned();
                    } else {
                        redact(&mut value, &replacements, &usernames);
                    }
                    redact(&mut key, &replacements, &usernames);
                    (key, value)
                })
                .collect();
        }
    }
}

fn is_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("file://")
        || (value.as_bytes().get(1) == Some(&b':')
            && value
                .as_bytes()
                .get(2)
                .is_some_and(|b| matches!(b, b'/' | b'\\')))
}

fn redact(text: &mut String, replacements: &[(String, String)], usernames: &[Regex]) {
    // Match credentials before replacing paths so quoted secrets are removed whole.
    *text = redact_secret_values(text);
    *text = URL.replace_all(text, "${1}<CREDENTIALS>@").into_owned();
    *text = TOKEN.replace_all(text, "<REDACTED>").into_owned();
    for (pattern, replacement) in replacements {
        if !pattern.is_empty() {
            *text = text.replace(pattern, replacement);
        }
    }
    *text = HOME_PATH.replace_all(text, "<HOME>").into_owned();
    for username in usernames {
        *text = username.replace_all(text, "<USER>").into_owned();
    }
    // Keep reports single-record and safe to display in a terminal. Render line
    // breaks as escapes; JSON/CSV encoders will perform their own format escaping.
    *text = text
        .chars()
        .flat_map(|c| match c {
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            c if c.is_control() => format!("\\u{{{:x}}}", c as u32).chars().collect(),
            c => vec![c],
        })
        .collect();
}

// Keep the quote state at the prefix: e.g. -H 'Authorization: Bearer VALUE'
// starts its sensitive value inside a quoted shell argument.
fn quote_before(text: &str) -> Option<char> {
    let mut quote = None;
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quote != Some('\'') {
            escaped = true;
        } else if Some(character) == quote {
            quote = None;
        } else if quote.is_none() && matches!(character, '\'' | '"') {
            quote = Some(character);
        }
    }
    quote
}

fn secret_value_end(text: &str, initial_quote: Option<char>) -> usize {
    let mut quote = initial_quote;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quote != Some('\'') {
            escaped = true;
        } else if Some(character) == quote {
            quote = None;
        } else if quote.is_none() && matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if quote.is_none()
            && (character.is_whitespace() || matches!(character, ';' | '&' | '|' | '<' | '>'))
        {
            return index;
        }
    }
    // An incomplete sensitive argument is redacted through the end of the text.
    text.len()
}

fn redact_secret_values(text: &str) -> String {
    let mut output = String::new();
    let mut copied = 0;
    for prefix in SECRET_PREFIX.find_iter(text) {
        if prefix.start() < copied {
            continue;
        }
        output.push_str(&text[copied..prefix.end()]);
        let outer_quote = quote_before(&text[..prefix.end()]);
        copied = prefix.end() + secret_value_end(&text[prefix.end()..], outer_quote);
        output.push_str("<REDACTED>");
        if let Some(quote) = outer_quote {
            output.push(quote);
        }
    }
    output.push_str(&text[copied..]);
    output
}

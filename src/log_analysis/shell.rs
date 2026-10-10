use super::{CommandCategory, MavenCommand};

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Boundary,
    Redirect,
}

// Preserve quoting and command boundaries without evaluating expansions. Refuse
// incomplete quoting, heredocs, substitutions, and backticks rather than infer
// executions from the text they contain.
fn tokens(command: &str) -> Option<Vec<Token>> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut plain_word = true;
    let mut quote = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && quote != Some('\'') {
            plain_word = false;
            let next = chars.next()?;
            if next != '\n' {
                if quote == Some('"') && !matches!(next, '$' | '`' | '"' | '\\') {
                    word.push('\\');
                }
                word.push(next);
                started = true;
            }
        } else if Some(c) == quote {
            quote = None;
        } else if quote == Some('\'') {
            word.push(c);
        } else if c == '`'
            || (c == '$' && chars.peek() == Some(&'('))
            || (quote.is_none() && c == '<' && chars.peek() == Some(&'<'))
        {
            return None;
        } else if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
            started = true;
            plain_word = false;
        } else if quote.is_none()
            && (matches!(c, '<' | '>') || (c == '&' && chars.peek() == Some(&'>')))
        {
            // A bare number adjoining the operator is a descriptor, not argv.
            // Keep quoted/escaped numbers and ordinary preceding arguments.
            if started && !(plain_word && word.bytes().all(|b| b.is_ascii_digit())) {
                result.push(Token::Word(std::mem::take(&mut word)));
            }
            word.clear();
            started = false;
            plain_word = true;
            if c == '&' {
                chars.next(); // &> or &>>
                if chars.peek() == Some(&'>') {
                    chars.next();
                }
            } else if chars.peek().is_some_and(|next| {
                matches!(
                    (c, *next),
                    ('>', '>') | ('>', '|') | ('>', '&') | ('<', '&') | ('<', '>')
                )
            }) {
                chars.next();
            }
            result.push(Token::Redirect);
        } else if quote.is_none() && c == '#' && !started {
            for next in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
            result.push(Token::Boundary);
        } else if quote.is_none() && (c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')'))
        {
            if started {
                result.push(Token::Word(std::mem::take(&mut word)));
                started = false;
                plain_word = true;
            }
            if !c.is_whitespace() || c == '\n' {
                result.push(Token::Boundary);
            }
        } else {
            word.push(c);
            started = true;
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        result.push(Token::Word(word));
    }
    Some(result)
}

fn executable_name(value: &str) -> &str {
    value.rsplit(['/', '\\']).next().unwrap_or(value)
}

fn invocations(command: &str) -> Vec<Vec<String>> {
    let Some(tokens) = tokens(command) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for segment in tokens.split(|token| *token == Token::Boundary) {
        let mut words = Vec::new();
        let mut skip_target = false;
        for token in segment {
            match token {
                Token::Redirect => skip_target = true,
                Token::Word(_) if skip_target => skip_target = false,
                Token::Word(word) => words.push(word.clone()),
                Token::Boundary => {}
            }
        }
        // A missing redirection operand cannot describe a complete invocation.
        if skip_target {
            continue;
        }
        let mut start = 0;
        while words.get(start).is_some_and(|w| assignment(w)) {
            start += 1;
        }
        if words
            .get(start)
            .is_some_and(|w| executable_name(w) == "env")
        {
            start += 1;
            while words.get(start).is_some_and(|w| {
                assignment(w) || matches!(w.as_str(), "-i" | "--ignore-environment" | "--")
            }) {
                start += 1;
            }
        }
        if start < words.len() {
            result.push(words[start..].to_vec());
        }
    }
    result
}

fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(key, _)| {
        !key.is_empty()
            && key
                .chars()
                .enumerate()
                .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
    })
}

pub fn parse_maven_command(command: &str) -> Option<MavenCommand> {
    let words = invocations(command).into_iter().find(|words| {
        matches!(
            executable_name(&words[0]),
            "mvn" | "mvn.cmd" | "mvnw" | "mvnw.cmd" | "mvnd" | "mvnd.cmd"
        )
    })?;
    let mut result = MavenCommand {
        executable: executable_name(&words[0]).to_owned(),
        ..MavenCommand::default()
    };
    let mut index = 1;
    while let Some(token) = words.get(index) {
        let mut argument = |short: &str, long: &str| -> Option<String> {
            if token == short || token == long {
                index += 1;
                words.get(index).cloned()
            } else {
                token
                    .strip_prefix(&format!("{long}="))
                    .or_else(|| token.strip_prefix(short).filter(|v| !v.is_empty()))
                    .map(|v| v.trim_start_matches('=').to_owned())
            }
        };
        if token == "-am" || token == "--also-make" {
            result.also_make = true;
        } else if token == "-amd" || token == "--also-make-dependents" {
            result.also_make_dependents = true;
        } else if let Some(value) = argument("-pl", "--projects") {
            result.modules.extend(csv(&value));
        } else if let Some(value) = argument("-rf", "--resume-from") {
            result.resume_from = Some(value);
        } else if let Some(value) = argument("-P", "--activate-profiles") {
            result.profiles.extend(csv(&value));
        } else if let Some(value) = argument("-D", "--define") {
            let (key, value) = value.split_once('=').unwrap_or((&value, "true"));
            if matches!(key, "test" | "it.test") {
                result.tests.extend(csv(value));
            }
            result.properties.insert(key.to_owned(), value.to_owned());
        } else if matches!(token.as_str(), "-ff" | "-fae" | "-fn") {
            // Reactor failure policy switches are not attached -f arguments.
        } else if let Some(value) = argument("-f", "--file") {
            result.pom_file = Some(value);
        } else if matches!(
            token.as_str(),
            "-s" | "--settings"
                | "-gs"
                | "--global-settings"
                | "-t"
                | "--toolchains"
                | "-gt"
                | "--global-toolchains"
                | "-T"
                | "--threads"
                | "-l"
                | "--log-file"
                | "-b"
                | "--builder"
                | "-emp"
                | "--encrypt-master-password"
                | "-ep"
                | "--encrypt-password"
        ) {
            index += 1;
        } else if LIFECYCLE.contains(&token.as_str()) {
            result.lifecycle_goals.push(token.clone());
        } else if !token.starts_with('-')
            && token.contains(':')
            && !token.contains('/')
            && !token.contains('=')
        {
            result.plugin_goals.push(token.clone());
        }
        index += 1;
    }
    Some(result)
}

const LIFECYCLE: &[&str] = &[
    "pre-clean",
    "clean",
    "post-clean",
    "validate",
    "initialize",
    "generate-sources",
    "process-sources",
    "generate-resources",
    "process-resources",
    "compile",
    "process-classes",
    "generate-test-sources",
    "process-test-sources",
    "generate-test-resources",
    "process-test-resources",
    "test-compile",
    "process-test-classes",
    "test",
    "prepare-package",
    "package",
    "pre-integration-test",
    "integration-test",
    "post-integration-test",
    "verify",
    "install",
    "deploy",
    "pre-site",
    "site",
    "post-site",
    "site-deploy",
];
fn csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(super) fn repository_category(command: &str) -> CommandCategory {
    let commands = invocations(command);
    let relevant: Vec<_> = commands
        .iter()
        .filter(|words| {
            matches!(
                executable_name(&words[0]),
                "find" | "jar" | "unzip" | "javap" | "grep" | "rg"
            )
        })
        .collect();
    if relevant.is_empty() {
        return CommandCategory::Shell;
    }
    let lower = relevant
        .iter()
        .flat_map(|words| words.iter().map(|word| word.to_ascii_lowercase()))
        .collect::<Vec<_>>()
        .join(" ");
    if relevant
        .iter()
        .any(|words| executable_name(&words[0]) == "javap")
        || lower.contains(".class")
    {
        CommandCategory::ClassInspection
    } else if lower.contains("pom.xml") || lower.contains(".pom") {
        CommandCategory::PomInspection
    } else if lower.contains("meta-inf")
        || lower.contains("resources")
        || (lower.contains(".jar")
            && [".xml", ".properties", ".yaml", ".yml", ".json"]
                .iter()
                .any(|s| lower.contains(s)))
    {
        CommandCategory::ResourceInspection
    } else if lower.contains(".jar")
        || lower.contains(".m2")
        || relevant
            .iter()
            .any(|words| matches!(executable_name(&words[0]), "jar" | "unzip"))
    {
        CommandCategory::JarInspection
    } else {
        CommandCategory::Shell
    }
}

#[cfg(test)]
#[path = "../../tests/unit/log_analysis_shell.rs"]
mod tests;

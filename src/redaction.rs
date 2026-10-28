//! Shared credential redaction for Maven output, structured diagnostics and MCP errors.
//!
//! Every pattern matches the original UTF-8 text through `regex`, so replacement ranges are
//! always character boundaries and arbitrary Unicode input cannot panic. Replacements never
//! match again, so redacting already redacted text is a no-op that counts nothing.

use regex::{Captures, Regex};
use std::{collections::BTreeMap, sync::LazyLock};

pub const REDACTED: &str = "<redacted>";
pub const CREDENTIALS: &str = "<credentials>";

/// Name fragments of secret-carrying keys, compared after removing separators and case.
const SENSITIVE_NAME_PARTS: [&str; 10] = [
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "credential",
    "apikey",
    "accesskey",
    "privatekey",
    "authorization",
];

const SENSITIVE_KEY: &str = r"[\w.-]*(?:password|passwd|passphrase|secret|token|credentials?|api[_-]?key|access[_-]?key|private[_-]?key)[\w.-]*";

// The complete header value is sensitive: the scheme (`Bearer`, `Basic`, `Digest`, ...) and
// every credential parameter through the end of the line, including quoted ones.
static AUTHORIZATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b((?:proxy-)?authorization["']?[ \t]*[:=][ \t]*["']?)[^\s<][^\r\n]*"#)
        .expect("constant authorization expression")
});
static XML_ELEMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?i)<({SENSITIVE_KEY})>[^<]+</"))
        .expect("constant sensitive XML element expression")
});
// `key=value`, `key = value`, `key: value` and quoted `"key":"value"`. A colon directly
// followed by an unquoted word is not a separator, so `PasswordTest.checks:42` stays intact.
// Quoted values honour backslash escapes; unquoted values run through the next whitespace,
// because punctuation such as `;`, `,` or `&` can be part of the secret.
static KEY_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    const QUOTED: &str = r#""(?:[^"\\\r\n]|\\.)*"?|'(?:[^'\\\r\n]|\\.)*'?"#;
    const VALUE: &str = r"<[^>\r\n]*>?\S*|\S+";
    Regex::new(&format!(
        r#"(?i)({SENSITIVE_KEY}["']?(?:[ \t]*=[ \t]*|[ \t]*:[ \t]+))({QUOTED}|{VALUE})|({SENSITIVE_KEY}["']?[ \t]*:)({QUOTED})"#
    ))
    .expect("constant sensitive key-value expression")
});
static URL_CREDENTIALS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b([a-z][a-z0-9+.-]*://)[^\s/?#'"<>]+@"#)
        .expect("constant URL credentials expression")
});
/// Self-identifying token formats that are secret wherever they appear.
pub(crate) static KNOWN_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:gh[pousr]_[A-Za-z0-9_]+|github_pat_[A-Za-z0-9_]+|sk-[A-Za-z0-9_-]{8,}|AKIA[A-Z0-9]{16}|eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+)\b")
        .expect("constant token expression")
});

/// Redacts credential values in free text and returns the number of actual replacements.
pub fn redact_sensitive_values(text: &str) -> (String, usize) {
    let mut count = 0;
    let text = AUTHORIZATION.replace_all(text, |captures: &Captures| {
        count += 1;
        format!("{}{REDACTED}", &captures[1])
    });
    let text = XML_ELEMENT.replace_all(&text, |captures: &Captures| {
        count += 1;
        format!("<{}>{REDACTED}</", &captures[1])
    });
    let text = KEY_VALUE.replace_all(&text, |captures: &Captures| {
        let (key, value) = match (captures.get(1), captures.get(2)) {
            (Some(key), Some(value)) => (key, value.as_str()),
            _ => (captures.get(3).expect("key group"), &captures[4]),
        };
        let closing = closing_brackets(&text[..key.start()], value);
        let secret = &value[..value.len() - closing.len()];
        if secret.is_empty() || secret == REDACTED {
            return captures[0].to_owned();
        }
        count += 1;
        format!("{}{REDACTED}{closing}", key.as_str())
    });
    let text = URL_CREDENTIALS.replace_all(&text, |captures: &Captures| {
        count += 1;
        format!("{}{CREDENTIALS}@", &captures[1])
    });
    let text = KNOWN_TOKEN.replace_all(&text, |_: &Captures| {
        count += 1;
        REDACTED
    });
    (text.into_owned(), count)
}

/// The trailing `)`, `]` and `}` of an unquoted value that close brackets opened earlier on
/// the same line, such as in `fails[password=value]`. They are kept instead of redacted.
fn closing_brackets<'a>(before: &str, value: &'a str) -> &'a str {
    if value.starts_with(['"', '\'']) {
        return "";
    }
    let line = before.rsplit(['\r', '\n']).next().unwrap_or_default();
    let mut open = [('(', ')'), ('[', ']'), ('{', '}')].map(|(opener, closer)| {
        let depth = |text: &str, bracket| text.matches(bracket).count();
        let unclosed = depth(line, opener).saturating_sub(depth(line, closer));
        (closer, unclosed)
    });
    let mut kept = value.len();
    while let Some(last) = value[..kept].chars().next_back() {
        let Some((_, unclosed)) = open
            .iter_mut()
            .find(|(closer, unclosed)| *closer == last && *unclosed > 0)
        else {
            break;
        };
        *unclosed -= 1;
        kept -= last.len_utf8();
    }
    &value[kept..]
}

/// Whether a property, header or option name normally carries a secret value.
pub fn is_sensitive_name(name: &str) -> bool {
    let normalized = name
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    SENSITIVE_NAME_PARTS
        .iter()
        .any(|part| normalized.contains(part))
}

/// Masks the values of secret-carrying properties and redacts embedded credentials, such as
/// URL user information, in every other value. Names and non-sensitive text stay unchanged.
pub fn redact_properties(properties: BTreeMap<String, String>) -> BTreeMap<String, String> {
    properties
        .into_iter()
        .map(|(name, value)| {
            let value = if value.is_empty() {
                value
            } else if is_sensitive_name(&name) {
                REDACTED.to_owned()
            } else {
                redact_sensitive_values(&value).0
            };
            (name, value)
        })
        .collect()
}

#[cfg(test)]
#[path = "../tests/unit/redaction.rs"]
mod tests;

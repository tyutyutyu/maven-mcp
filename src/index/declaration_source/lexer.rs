//! Java and Kotlin lexer that separates code tokens from comments and literals.

use super::SourceLanguage;

/// Nesting limit of Kotlin string templates inside string templates.
const MAX_TEMPLATE_DEPTH: usize = 32;
/// Open brackets searched below the top of the stack to recover from a stray closer.
const MAX_BRACKET_RECOVERY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TokenKind {
    Identifier,
    Punctuation,
    Literal,
}

/// A code token; `line` and `end_line` are zero-based lines of its first and last byte.
#[derive(Debug, Clone, Copy)]
pub(super) struct Token {
    pub(super) kind: TokenKind,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) line: usize,
    pub(super) end_line: usize,
}

pub(super) struct Lexer<'a> {
    source: &'a str,
    position: usize,
    line: usize,
    language: SourceLanguage,
}

impl<'a> Lexer<'a> {
    pub(super) fn tokenize(source: &'a str, language: SourceLanguage) -> Vec<Token> {
        let mut lexer = Self {
            source,
            position: 0,
            line: 0,
            language,
        };
        let mut tokens = Vec::new();
        while let Some(token) = lexer.next_token(0) {
            tokens.push(token);
        }
        tokens
    }

    fn rest(&self) -> &'a str {
        &self.source[self.position..]
    }

    fn starts_with(&self, text: &str) -> bool {
        self.rest().starts_with(text)
    }

    fn current(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) {
        if let Some(character) = self.current() {
            self.position += character.len_utf8();
            if character == '\n' {
                self.line += 1;
            }
        }
    }

    /// Skips `count` ASCII characters that are known not to contain a newline.
    fn bump_ascii(&mut self, count: usize) {
        self.position += count;
    }

    fn next_token(&mut self, template_depth: usize) -> Option<Token> {
        self.skip_trivia();
        let start = self.position;
        let line = self.line;
        let character = self.current()?;
        let kind = match character {
            '"' => {
                self.skip_string(template_depth);
                TokenKind::Literal
            }
            '\'' => {
                self.skip_quoted('\'');
                TokenKind::Literal
            }
            '`' if self.language == SourceLanguage::Kotlin => {
                self.skip_quoted('`');
                TokenKind::Identifier
            }
            _ if character.is_ascii_digit() => {
                self.skip_number();
                TokenKind::Literal
            }
            _ if is_identifier_char(character) => {
                while self.current().is_some_and(is_identifier_char) {
                    self.bump();
                }
                TokenKind::Identifier
            }
            _ => {
                if self.starts_with("->") {
                    self.bump_ascii(2);
                } else {
                    self.bump();
                }
                TokenKind::Punctuation
            }
        };
        Some(Token {
            kind,
            start,
            end: self.position,
            line,
            end_line: self.line,
        })
    }

    fn skip_trivia(&mut self) {
        loop {
            if self.current().is_some_and(char::is_whitespace) {
                self.bump();
            } else if self.starts_with("//") {
                while self.current().is_some_and(|character| character != '\n') {
                    self.bump();
                }
            } else if self.starts_with("/*") {
                self.skip_block_comment();
            } else {
                return;
            }
        }
    }

    /// Kotlin block comments nest; Java block comments end at the first `*/`.
    fn skip_block_comment(&mut self) {
        let nested = self.language == SourceLanguage::Kotlin;
        let mut depth = 0usize;
        while self.current().is_some() {
            if self.starts_with("/*") && (depth == 0 || nested) {
                self.bump_ascii(2);
                depth += 1;
            } else if self.starts_with("*/") {
                self.bump_ascii(2);
                depth -= 1;
                if depth == 0 {
                    return;
                }
            } else {
                self.bump();
            }
        }
    }

    fn skip_string(&mut self, template_depth: usize) {
        if self.starts_with("\"\"\"") {
            self.bump_ascii(3);
            match self.language {
                SourceLanguage::Java => self.skip_text_block(),
                SourceLanguage::Kotlin => self.skip_raw_string(template_depth),
            }
            return;
        }
        self.bump_ascii(1);
        while let Some(character) = self.current() {
            match character {
                '\n' => return,
                '\\' => {
                    self.bump();
                    self.bump();
                }
                '"' => {
                    self.bump();
                    return;
                }
                '$' if self.is_template_start() => self.skip_template(template_depth),
                _ => self.bump(),
            }
        }
    }

    fn skip_text_block(&mut self) {
        while let Some(character) = self.current() {
            if character == '\\' {
                self.bump();
                self.bump();
            } else if self.starts_with("\"\"\"") {
                self.bump_ascii(3);
                return;
            } else {
                self.bump();
            }
        }
    }

    /// A Kotlin raw string has no escapes; extra quotes before the closing ones are content.
    fn skip_raw_string(&mut self, template_depth: usize) {
        while self.current().is_some() {
            if self.starts_with("\"\"\"") {
                while self.current() == Some('"') {
                    self.bump();
                }
                return;
            }
            if self.is_template_start() {
                self.skip_template(template_depth);
            } else {
                self.bump();
            }
        }
    }

    fn is_template_start(&self) -> bool {
        self.language == SourceLanguage::Kotlin && self.starts_with("${")
    }

    /// Skips the code of a `${...}` template, whose braces and strings may nest.
    fn skip_template(&mut self, template_depth: usize) {
        self.bump_ascii(2);
        if template_depth >= MAX_TEMPLATE_DEPTH {
            return;
        }
        let mut depth = 1usize;
        while let Some(token) = self.next_token(template_depth + 1) {
            if token.kind != TokenKind::Punctuation {
                continue;
            }
            match &self.source[token.start..token.end] {
                "{" => depth += 1,
                "}" => {
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    fn skip_quoted(&mut self, quote: char) {
        self.bump();
        while let Some(character) = self.current() {
            if character == '\n' {
                return;
            }
            self.bump();
            if character == '\\' {
                self.bump();
            } else if character == quote {
                return;
            }
        }
    }

    fn skip_number(&mut self) {
        loop {
            let mut characters = self.rest().chars();
            match characters.next() {
                Some(character) if is_identifier_char(character) => self.bump(),
                Some('.') if characters.next().is_some_and(|next| next.is_ascii_digit()) => {
                    self.bump();
                }
                _ => return,
            }
        }
    }
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

/// Links every bracket to its partner; unbalanced brackets stay unlinked.
pub(super) fn match_brackets(source: &str, tokens: &[Token]) -> Vec<Option<usize>> {
    let mut matching = vec![None; tokens.len()];
    let mut open: Vec<(usize, &str)> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Punctuation {
            continue;
        }
        let text = &source[token.start..token.end];
        let opener = match text {
            "(" | "[" | "{" => {
                open.push((index, text));
                continue;
            }
            ")" => "(",
            "]" => "[",
            "}" => "{",
            _ => continue,
        };
        let searched = open.len().saturating_sub(MAX_BRACKET_RECOVERY);
        if let Some(position) = open[searched..]
            .iter()
            .rposition(|(_, value)| *value == opener)
            .map(|position| searched + position)
        {
            let (start, _) = open[position];
            open.truncate(position);
            matching[start] = Some(index);
            matching[index] = Some(start);
        }
    }
    matching
}

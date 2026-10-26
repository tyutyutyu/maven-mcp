//! Member outline of Java and Kotlin type bodies: declaration kinds and boundaries.

use super::{
    FunctionDeclaration, Member, MemberKind, SourceLanguage, SourceOutline, TypeDeclaration,
};

const JAVA_MODIFIERS: &[&str] = &[
    "public",
    "protected",
    "private",
    "static",
    "final",
    "abstract",
    "transient",
    "volatile",
    "synchronized",
    "native",
    "strictfp",
    "default",
    "sealed",
];
pub(super) const KOTLIN_MODIFIERS: &[&str] = &[
    "public",
    "protected",
    "private",
    "internal",
    "abstract",
    "final",
    "open",
    "override",
    "sealed",
    "data",
    "inner",
    "value",
    "annotation",
    "companion",
    "const",
    "lateinit",
    "inline",
    "noinline",
    "crossinline",
    "suspend",
    "tailrec",
    "operator",
    "infix",
    "external",
    "expect",
    "actual",
    "vararg",
];
/// Tokens that keep a Kotlin statement open when they end a line.
///
/// A line-ending `>` is decided separately, because it may close type arguments.
const KOTLIN_CONTINUING_SUFFIXES: &[&str] = &[
    "=", ",", ".", ":", "->", "+", "-", "*", "/", "%", "&", "|", "<",
];
/// Keywords whose parenthesized condition keeps a Kotlin statement open when it ends a line.
const KOTLIN_CONDITION_WORDS: &[&str] = &["if", "while", "for"];
/// Tokens that continue the previous Kotlin statement when they start a line.
const KOTLIN_CONTINUING_PREFIXES: &[&str] = &[".", "?", ":", "=", "{", "&", "|"];
const KOTLIN_CONTINUING_WORDS: &[&str] = &["where", "by", "else", "catch", "finally"];

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Modifiers {
    pub(super) is_enum: bool,
    pub(super) is_suspend: bool,
    pub(super) is_companion: bool,
}

/// Start of a Java method or constructor: the member start, the first token after the
/// modifiers and type parameters, and the type parameters.
struct JavaHeader {
    start: usize,
    first: usize,
    type_parameters: Option<(usize, usize)>,
}

impl SourceOutline<'_> {
    pub(super) fn members_of(&self, declaration: &TypeDeclaration, member: &Member) -> Vec<Member> {
        let mut members = Vec::new();
        if let Some((open, close)) = declaration.primary_parameters {
            if !declaration.is_record {
                members.push(Member {
                    start: member.start,
                    end: close,
                    kind: MemberKind::Function(FunctionDeclaration {
                        name: declaration.name.clone(),
                        is_constructor: true,
                        is_suspend: false,
                        type_parameters: None,
                        receiver: None,
                        parameters: (open, close),
                    }),
                });
            }
            members.extend(self.header_fields(open, close, declaration.is_record));
        }
        if let Some((open, close)) = declaration.body {
            members.extend(self.scan_members(
                open + 1,
                close,
                Some(&declaration.name),
                declaration.is_enum,
            ));
        }
        members
    }

    /// Java record components and Kotlin `val`/`var` primary constructor parameters.
    fn header_fields(&self, open: usize, close: usize, is_record: bool) -> Vec<Member> {
        self.split_list(open, close)
            .into_iter()
            .filter_map(|(start, end)| {
                let (index, _) = self.skip_modifiers(start, end);
                let name_index = match self.word(index) {
                    Some("val" | "var") => index + 1,
                    _ if is_record => self.java_parameter_name_index(start, end)?,
                    _ => return None,
                };
                let name = self.identifier(name_index)?;
                Some(Member {
                    start,
                    end: end - 1,
                    kind: MemberKind::Field(vec![name.to_owned()]),
                })
            })
            .collect()
    }

    pub(super) fn scan_members(
        &self,
        start: usize,
        end: usize,
        owner: Option<&str>,
        is_enum: bool,
    ) -> Vec<Member> {
        let mut members = Vec::new();
        let mut index = start;
        if is_enum {
            index = self.scan_enum_entries(index, end, &mut members);
        }
        while index < end {
            let member = self.member_at(index, end, owner);
            index = member.end.max(index) + 1;
            members.push(member);
        }
        members
    }

    fn scan_enum_entries(&self, mut index: usize, end: usize, members: &mut Vec<Member>) -> usize {
        while index < end {
            if self.is_punct(index, ";") {
                return index + 1;
            }
            let name_index = self.skip_annotations(index, end);
            let Some(name) = self.identifier(name_index).filter(|_| name_index < end) else {
                return index;
            };
            let mut next = name_index + 1;
            let follows_entry = next >= end
                || [",", ";", "(", "{"]
                    .iter()
                    .any(|value| self.is_punct(next, value));
            if !follows_entry {
                return index;
            }
            if self.is_punct(next, "(") {
                next = self.group_end(next, end) + 1;
            }
            if next < end && self.is_punct(next, "{") {
                next = self.group_end(next, end) + 1;
            }
            members.push(Member {
                start: index,
                end: next - 1,
                kind: MemberKind::Field(vec![name.to_owned()]),
            });
            if next < end && self.is_punct(next, ",") {
                next += 1;
            }
            index = next;
        }
        index
    }

    fn member_at(&self, start: usize, end: usize, owner: Option<&str>) -> Member {
        let (index, modifiers) = self.skip_modifiers(start, end);
        if index >= end {
            return Member {
                start,
                end: end - 1,
                kind: MemberKind::Other,
            };
        }
        if let Some(keyword) = self.type_keyword(index) {
            return self.type_member(start, keyword, end, modifiers);
        }
        match self.language {
            SourceLanguage::Java => self.java_member(start, index, end, owner),
            SourceLanguage::Kotlin => self.kotlin_member(start, index, end, owner, modifiers),
        }
    }

    /// Returns the index of the type keyword of a type declaration starting at `index`.
    fn type_keyword(&self, index: usize) -> Option<usize> {
        if self.is_punct(index, "@") && self.word(index + 1) == Some("interface") {
            return Some(index + 1);
        }
        let keyword = match (self.language, self.word(index)?) {
            (_, "class" | "interface") | (SourceLanguage::Java, "enum") => index,
            (SourceLanguage::Java, "record")
                if self.identifier(index + 1).is_some()
                    && (self.is_punct(index + 2, "(") || self.is_punct(index + 2, "<")) =>
            {
                index
            }
            (SourceLanguage::Kotlin, "object") => index,
            (SourceLanguage::Kotlin, "fun") if self.word(index + 1) == Some("interface") => {
                index + 1
            }
            _ => return None,
        };
        Some(keyword)
    }

    fn type_member(
        &self,
        start: usize,
        keyword: usize,
        end: usize,
        modifiers: Modifiers,
    ) -> Member {
        let keyword_text = self.word(keyword).unwrap_or_default();
        let is_companion = modifiers.is_companion && keyword_text == "object";
        let mut index = keyword + 1;
        let name = match self.identifier(index).filter(|_| index < end) {
            Some(name) => {
                index += 1;
                name.to_owned()
            }
            None if is_companion => "Companion".to_owned(),
            None => String::new(),
        };
        let type_parameters = if self.is_punct(index, "<") {
            let open = index;
            let close = self.angle_end(open, end);
            index = close + 1;
            Some((open, close))
        } else {
            None
        };
        let is_record = keyword_text == "record";
        let primary_parameters = self.primary_parameters(index, end, is_record);
        if let Some((_, close)) = primary_parameters {
            index = close + 1;
        }
        let (body, last) = self.find_body(keyword, index - 1, end);
        Member {
            start,
            end: last.max(keyword),
            kind: MemberKind::Type(TypeDeclaration {
                name,
                is_enum: modifiers.is_enum || keyword_text == "enum",
                is_record,
                is_companion,
                type_parameters,
                primary_parameters,
                body,
            }),
        }
    }

    fn primary_parameters(
        &self,
        index: usize,
        end: usize,
        is_record: bool,
    ) -> Option<(usize, usize)> {
        let open = match self.language {
            SourceLanguage::Java if is_record => index,
            SourceLanguage::Java => return None,
            SourceLanguage::Kotlin => {
                let (after_modifiers, _) = self.skip_modifiers(index, end);
                if self.word(after_modifiers) == Some("constructor") {
                    after_modifiers + 1
                } else if after_modifiers == index {
                    index
                } else {
                    return None;
                }
            }
        };
        (open < end && self.is_punct(open, "(")).then(|| (open, self.group_end(open, end)))
    }

    fn java_member(&self, start: usize, index: usize, end: usize, owner: Option<&str>) -> Member {
        if self.is_punct(index, "{") || self.is_punct(index, ";") {
            return Member {
                start,
                end: self.group_end(index, end),
                kind: MemberKind::Other,
            };
        }
        let type_parameters = self
            .is_punct(index, "<")
            .then(|| (index, self.angle_end(index, end)));
        let first = type_parameters.map_or(index, |(_, close)| close + 1);
        let mut cursor = first;
        let mut angle_depth = 0usize;
        while cursor < end {
            if self.is_punct(cursor, "<") {
                angle_depth += 1;
            } else if self.is_punct(cursor, ">") {
                angle_depth = angle_depth.saturating_sub(1);
            } else if angle_depth == 0 {
                if self.is_punct(cursor, "(") {
                    let header = JavaHeader {
                        start,
                        first,
                        type_parameters,
                    };
                    return self.java_function(header, cursor, end, owner);
                }
                if [",", "=", ";"]
                    .iter()
                    .any(|value| self.is_punct(cursor, value))
                {
                    return self.java_field(start, first, cursor, end);
                }
                if self.is_punct(cursor, "{") || self.is_closer(cursor) {
                    break;
                }
            }
            cursor = self.group_end(cursor, end) + 1;
        }
        Member {
            start,
            end: self.java_statement_end(index, end),
            kind: MemberKind::Other,
        }
    }

    fn java_function(
        &self,
        header: JavaHeader,
        open: usize,
        end: usize,
        owner: Option<&str>,
    ) -> Member {
        let close = self.group_end(open, end);
        let (_, last) = self.find_body(header.start, close, end);
        let name = (open > header.first)
            .then(|| self.identifier(open - 1))
            .flatten();
        let Some(name) = name else {
            return Member {
                start: header.start,
                end: last,
                kind: MemberKind::Other,
            };
        };
        Member {
            start: header.start,
            end: last,
            kind: MemberKind::Function(FunctionDeclaration {
                name: name.to_owned(),
                is_constructor: open - 1 == header.first && owner == Some(name),
                is_suspend: false,
                type_parameters: header.type_parameters,
                receiver: None,
                parameters: (open, close),
            }),
        }
    }

    fn java_field(&self, start: usize, first: usize, stop: usize, end: usize) -> Member {
        let last = self.java_field_end(stop, end);
        let mut names = Vec::new();
        names.extend(self.declarator_name_before(stop, first));
        let mut index = stop;
        while index < last {
            if self.is_punct(index, ",") {
                let declarator = self.identifier(index + 1).filter(|_| {
                    ["=", ",", ";", "["]
                        .iter()
                        .any(|value| self.is_punct(index + 2, value))
                });
                names.extend(declarator.map(str::to_owned));
            }
            index = self.group_end(index, end) + 1;
        }
        Member {
            start,
            end: last,
            kind: MemberKind::Field(names),
        }
    }

    fn declarator_name_before(&self, stop: usize, first: usize) -> Option<String> {
        let mut index = stop.checked_sub(1)?;
        while index > first && self.is_punct(index, "]") {
            index = index.checked_sub(2)?;
        }
        (index > first)
            .then(|| self.identifier(index))
            .flatten()
            .map(str::to_owned)
    }

    fn java_field_end(&self, from: usize, end: usize) -> usize {
        let mut index = from;
        while index < end {
            if self.is_punct(index, ";") {
                return index;
            }
            if self.is_closer(index) {
                return index.saturating_sub(1).max(from);
            }
            index = self.group_end(index, end) + 1;
        }
        end - 1
    }

    fn java_statement_end(&self, from: usize, end: usize) -> usize {
        let mut index = from;
        while index < end {
            if self.is_punct(index, ";") || self.is_punct(index, "{") {
                return self.group_end(index, end);
            }
            if self.is_closer(index) {
                return index.saturating_sub(1).max(from);
            }
            index = self.group_end(index, end) + 1;
        }
        end - 1
    }

    fn kotlin_member(
        &self,
        start: usize,
        index: usize,
        end: usize,
        owner: Option<&str>,
        modifiers: Modifiers,
    ) -> Member {
        let other = |last: usize| Member {
            start,
            end: last,
            kind: MemberKind::Other,
        };
        match self.word(index) {
            Some("fun") => self.kotlin_function(start, index, end, modifiers.is_suspend),
            Some("val" | "var") => self.kotlin_property(start, index, end),
            Some("constructor") if self.is_punct(index + 1, "(") => {
                let close = self.group_end(index + 1, end);
                let (_, last) = self.find_body(index, close, end);
                Member {
                    start,
                    end: last,
                    kind: MemberKind::Function(FunctionDeclaration {
                        name: owner.unwrap_or_default().to_owned(),
                        is_constructor: true,
                        is_suspend: false,
                        type_parameters: None,
                        receiver: None,
                        parameters: (index + 1, close),
                    }),
                }
            }
            Some("init") if self.is_punct(index + 1, "{") => other(self.group_end(index + 1, end)),
            _ => other(self.statement_end(index, end, false)),
        }
    }

    fn kotlin_function(
        &self,
        start: usize,
        keyword: usize,
        end: usize,
        is_suspend: bool,
    ) -> Member {
        let other = |last: usize| Member {
            start,
            end: last,
            kind: MemberKind::Other,
        };
        let mut cursor = keyword + 1;
        let type_parameters = if self.is_punct(cursor, "<") {
            let open = cursor;
            let close = self.angle_end(open, end);
            cursor = close + 1;
            Some((open, close))
        } else {
            None
        };
        let receiver_start = cursor;
        while cursor < end && !(self.identifier(cursor).is_some() && self.is_punct(cursor + 1, "("))
        {
            let stops = ["{", "=", ";"]
                .iter()
                .any(|value| self.is_punct(cursor, value));
            if stops || self.is_closer(cursor) {
                return other(self.statement_end(keyword, end, false));
            }
            cursor = if self.is_punct(cursor, "<") {
                self.angle_end(cursor, end) + 1
            } else {
                self.group_end(cursor, end) + 1
            };
        }
        let Some(name) = self.identifier(cursor).filter(|_| cursor < end) else {
            return other(end - 1);
        };
        let open = cursor + 1;
        let close = self.group_end(open, end);
        let (_, last) = self.find_body(keyword, close, end);
        Member {
            start,
            end: last,
            kind: MemberKind::Function(FunctionDeclaration {
                name: name.to_owned(),
                is_constructor: false,
                is_suspend,
                type_parameters,
                receiver: (cursor > receiver_start + 1).then_some((receiver_start, cursor - 1)),
                parameters: (open, close),
            }),
        }
    }

    fn kotlin_property(&self, start: usize, keyword: usize, end: usize) -> Member {
        let last = self.statement_end(keyword, end, true);
        let mut name = None;
        let mut cursor = keyword + 1;
        while cursor <= last {
            let stops = [":", "="].iter().any(|value| self.is_punct(cursor, value))
                || self.word(cursor) == Some("by");
            if stops {
                break;
            }
            if self.is_punct(cursor, "<") {
                cursor = self.angle_end(cursor, end) + 1;
                continue;
            }
            if let Some(identifier) = self.identifier(cursor) {
                name = Some(identifier.to_owned());
            }
            cursor = self.group_end(cursor, end) + 1;
        }
        Member {
            start,
            end: last,
            kind: MemberKind::Field(name.into_iter().collect()),
        }
    }

    /// Finds the body of a declaration whose header runs from `start` to `last`.
    ///
    /// Returns the body braces and the last token of the declaration.
    fn find_body(&self, start: usize, last: usize, end: usize) -> (Option<(usize, usize)>, usize) {
        let kotlin = self.language == SourceLanguage::Kotlin;
        let mut previous = last;
        let mut index = last + 1;
        while index < end {
            if kotlin && self.ends_statement(start, previous, index, false) {
                return (None, previous);
            }
            if self.is_punct(index, "{") {
                let close = self.closing(index).filter(|close| *close < end);
                return (
                    Some((index, close.unwrap_or(end))),
                    close.unwrap_or(end - 1),
                );
            }
            if self.is_punct(index, ";") {
                return (None, index);
            }
            if self.is_closer(index) {
                return (None, previous);
            }
            if kotlin && self.is_punct(index, "=") {
                return (None, self.statement_end(index, end, false));
            }
            previous = self.group_end(index, end);
            index = previous + 1;
        }
        (None, previous.min(end - 1))
    }

    /// Last token of a Kotlin statement that starts at `from`, by the newline rules.
    fn statement_end(&self, from: usize, end: usize, is_property: bool) -> usize {
        let mut previous = self.group_end(from, end);
        let mut index = previous + 1;
        while index < end {
            if self.is_closer(index) || self.ends_statement(from, previous, index, is_property) {
                return previous;
            }
            if self.is_punct(index, ";") {
                return index;
            }
            previous = self.group_end(index, end);
            index = previous + 1;
        }
        previous.min(end - 1)
    }

    /// Whether a newline between `previous` and `next` ends the statement that starts at `from`.
    fn ends_statement(&self, from: usize, previous: usize, next: usize, is_property: bool) -> bool {
        self.tokens[next].line > self.tokens[previous].end_line
            && !self.continues_statement(from, previous, next, is_property)
    }

    fn continues_statement(
        &self,
        from: usize,
        previous: usize,
        next: usize,
        is_property: bool,
    ) -> bool {
        let continues = self.line_end_continues(from, previous)
            || KOTLIN_CONTINUING_PREFIXES
                .iter()
                .any(|value| self.is_punct(next, value))
            || self
                .word(next)
                .is_some_and(|word| KOTLIN_CONTINUING_WORDS.contains(&word));
        if continues || !is_property {
            return continues;
        }
        let (accessor, _) = self.skip_modifiers(next, self.tokens.len());
        matches!(self.word(accessor), Some("get" | "set"))
            && (accessor + 1 >= self.tokens.len()
                || self.is_punct(accessor + 1, "(")
                || self.is_punct(accessor + 1, "=")
                || self.tokens[accessor + 1].line > self.tokens[accessor].end_line)
    }

    /// Whether a line ending at `previous` leaves the statement that starts at `from` open: a
    /// binary operator, a trailing `else`, or the condition of an `if`, `while` or `for`.
    fn line_end_continues(&self, from: usize, previous: usize) -> bool {
        if KOTLIN_CONTINUING_SUFFIXES
            .iter()
            .any(|value| self.is_punct(previous, value))
            || self.word(previous) == Some("else")
        {
            return true;
        }
        if self.is_punct(previous, ">") {
            return !self.closes_type_arguments(from, previous);
        }
        self.is_punct(previous, ")")
            && self
                .closing(previous)
                .and_then(|open| open.checked_sub(1))
                .and_then(|keyword| self.word(keyword))
                .is_some_and(|keyword| KOTLIN_CONDITION_WORDS.contains(&keyword))
    }

    /// Whether the `>` at `close` closes a `<` opened since `from`, so it ends type arguments
    /// rather than being a comparison.
    fn closes_type_arguments(&self, from: usize, close: usize) -> bool {
        let mut depth = 0usize;
        let mut index = from;
        while index < close {
            if self.is_punct(index, "<") {
                depth += 1;
            } else if self.is_punct(index, ">") {
                depth = depth.saturating_sub(1);
            }
            index = self.group_end(index, close) + 1;
        }
        depth > 0
    }

    pub(super) fn skip_modifiers(&self, mut index: usize, end: usize) -> (usize, Modifiers) {
        let mut modifiers = Modifiers::default();
        loop {
            index = self.skip_annotations(index, end);
            let Some(word) = self.word(index).filter(|_| index < end) else {
                return (index, modifiers);
            };
            if word == "non"
                && self.is_punct(index + 1, "-")
                && self.word(index + 2) == Some("sealed")
            {
                index += 3;
                continue;
            }
            let known = match self.language {
                SourceLanguage::Java => JAVA_MODIFIERS.contains(&word),
                SourceLanguage::Kotlin => {
                    KOTLIN_MODIFIERS.contains(&word)
                        || (word == "enum" && self.word(index + 1) == Some("class"))
                }
            };
            if !known {
                return (index, modifiers);
            }
            modifiers.is_enum |= word == "enum";
            modifiers.is_suspend |= word == "suspend";
            modifiers.is_companion |= word == "companion";
            index += 1;
        }
    }

    pub(super) fn skip_annotations(&self, mut index: usize, end: usize) -> usize {
        while index < end && self.is_punct(index, "@") && self.word(index + 1) != Some("interface")
        {
            index += 1;
            if self.language == SourceLanguage::Kotlin
                && self.identifier(index).is_some()
                && self.is_punct(index + 1, ":")
            {
                index += 2;
            }
            if self.is_punct(index, "[") {
                index = self.group_end(index, end) + 1;
                continue;
            }
            while self.identifier(index).is_some() {
                index += 1;
                if self.is_punct(index, ".") && self.identifier(index + 1).is_some() {
                    index += 1;
                } else {
                    break;
                }
            }
            if self.is_punct(index, "(") {
                index = self.group_end(index, end) + 1;
            }
        }
        index
    }
}

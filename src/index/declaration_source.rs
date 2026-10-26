//! Lexical outline of one Java or Kotlin source file for declaration slicing.
//!
//! The lexer separates code from comments and literals (Java text blocks, Kotlin
//! raw strings and string templates included), and the outline walks balanced
//! brackets, so a brace or semicolon inside a comment or literal never moves a
//! declaration boundary. It is deliberately not a compiler: overloads are matched
//! against the classfile descriptor through erased simple type names, and a
//! declaration that cannot be proven unique is reported as unresolved instead of
//! being guessed.

mod erasure;
mod lexer;
mod members;

use std::collections::BTreeMap;

use super::{DeclarationCandidate, DeclarationKind, UnresolvedDeclarationReason};
use erasure::{DescriptorMatch, descriptor_parameters};
use lexer::{Lexer, Token, TokenKind, match_brackets};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceLanguage {
    Java,
    Kotlin,
}

impl SourceLanguage {
    pub(super) fn of_entry(entry: &str) -> Self {
        if entry.ends_with(".kt") {
            Self::Kotlin
        } else {
            Self::Java
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct SourceSlice {
    pub start_line: usize,
    pub end_line: usize,
    pub source: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SliceOutcome {
    Found(SourceSlice),
    Unresolved {
        reason: UnresolvedDeclarationReason,
        candidate_lines: Vec<usize>,
    },
}

impl SliceOutcome {
    fn unresolved(reason: UnresolvedDeclarationReason, candidate_lines: Vec<usize>) -> Self {
        Self::Unresolved {
            reason,
            candidate_lines,
        }
    }

    fn not_found() -> Self {
        Self::unresolved(UnresolvedDeclarationReason::NotFound, Vec::new())
    }
}

#[derive(Debug, Clone)]
struct Member {
    start: usize,
    end: usize,
    kind: MemberKind,
}

#[derive(Debug, Clone)]
enum MemberKind {
    Type(TypeDeclaration),
    Function(FunctionDeclaration),
    Field(Vec<String>),
    Other,
}

#[derive(Debug, Clone)]
struct TypeDeclaration {
    name: String,
    is_enum: bool,
    is_record: bool,
    is_companion: bool,
    type_parameters: Option<(usize, usize)>,
    /// Kotlin primary constructor or Java record header, as open and close parentheses.
    primary_parameters: Option<(usize, usize)>,
    /// Body braces; the close is the token count when a truncated body is not closed.
    body: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
struct FunctionDeclaration {
    name: String,
    is_constructor: bool,
    is_suspend: bool,
    type_parameters: Option<(usize, usize)>,
    /// Kotlin extension receiver type as a half-open token range.
    receiver: Option<(usize, usize)>,
    parameters: (usize, usize),
}

/// Type variables in scope, mapped to the half-open token range of their first bound.
type TypeVariables = BTreeMap<String, Option<(usize, usize)>>;

/// The enclosing type chain of a located type, outermost first.
type Owners = [(TypeDeclaration, Member)];

pub(super) struct SourceOutline<'a> {
    source: &'a str,
    language: SourceLanguage,
    tokens: Vec<Token>,
    matching: Vec<Option<usize>>,
}

impl<'a> SourceOutline<'a> {
    pub(super) fn parse(source: &'a str, language: SourceLanguage) -> Self {
        let tokens = Lexer::tokenize(source, language);
        let matching = match_brackets(source, &tokens);
        Self {
            source,
            language,
            tokens,
            matching,
        }
    }

    /// Locates the declaration of `candidate` in the type named by the binary `class_name`.
    ///
    /// A compiler-generated (bridge or synthetic) method has no source declaration of its
    /// own, so it is never matched to a same-named source method.
    pub(super) fn locate(
        &self,
        class_name: &str,
        candidate: &DeclarationCandidate,
        is_compiler_generated: bool,
    ) -> SliceOutcome {
        let Some(path) = nested_type_path(class_name) else {
            return SliceOutcome::not_found();
        };
        let mut members = self.scan_members(0, self.tokens.len(), None, false);
        let mut owners: Vec<(TypeDeclaration, Member)> = Vec::new();
        for segment in path {
            let found = members
                .iter()
                .filter(|member| {
                    matches!(&member.kind, MemberKind::Type(declaration) if declaration.name == segment)
                })
                .collect::<Vec<_>>();
            let [member] = found.as_slice() else {
                return self.unresolved_members(&found);
            };
            let MemberKind::Type(declaration) = &member.kind else {
                return SliceOutcome::not_found();
            };
            let owner = (declaration.clone(), (*member).clone());
            members = self.members_of(&owner.0, &owner.1);
            owners.push(owner);
        }
        let Some((_, owner_member)) = owners.last() else {
            return SliceOutcome::not_found();
        };
        match candidate.kind {
            DeclarationKind::Class => {
                SliceOutcome::Found(self.slice(owner_member.start, owner_member.end))
            }
            DeclarationKind::Field => self.locate_field(&members, &candidate.name),
            DeclarationKind::Method => {
                self.locate_function(&members, &owners, candidate, is_compiler_generated)
            }
        }
    }

    fn locate_field(&self, members: &[Member], name: &str) -> SliceOutcome {
        let is_field = |member: &Member| matches!(&member.kind, MemberKind::Field(names) if names.iter().any(|value| value == name));
        let found = self.direct_or_companion(members, is_field);
        match found.as_slice() {
            [member] => SliceOutcome::Found(self.slice(member.start, member.end)),
            _ => self.unresolved_members(&found.iter().collect::<Vec<_>>()),
        }
    }

    fn locate_function(
        &self,
        members: &[Member],
        owners: &Owners,
        candidate: &DeclarationCandidate,
        is_compiler_generated: bool,
    ) -> SliceOutcome {
        let is_function = |member: &Member| matches!(&member.kind, MemberKind::Function(function) if function.name == candidate.name);
        let found = self.direct_or_companion(members, is_function);
        if found.is_empty() {
            return SliceOutcome::not_found();
        }
        let unproven = || {
            SliceOutcome::unresolved(
                UnresolvedDeclarationReason::Unproven,
                self.member_lines(&found.iter().collect::<Vec<_>>()),
            )
        };
        if is_compiler_generated {
            return unproven();
        }
        let Some(descriptor) = candidate.descriptor.as_deref() else {
            return match found.as_slice() {
                [member] => SliceOutcome::Found(self.slice(member.start, member.end)),
                _ => self.unresolved_members(&found.iter().collect::<Vec<_>>()),
            };
        };
        let Some(expected) = descriptor_parameters(descriptor) else {
            return unproven();
        };
        let mut proven = Vec::new();
        let mut possible = Vec::new();
        for member in &found {
            match self.function_matches(member, owners, &expected) {
                DescriptorMatch::Proven => proven.push(member),
                DescriptorMatch::Possible => possible.push(member),
                DescriptorMatch::Excluded => {}
            }
        }
        match (proven.as_slice(), possible.is_empty()) {
            ([member], true) => SliceOutcome::Found(self.slice(member.start, member.end)),
            // No match, or only declarations whose parameter list was cut off: none is proven.
            ([], _) => unproven(),
            _ => self.unresolved_members(&[proven, possible].concat()),
        }
    }

    /// Direct members accepted by `accept`, or those of a Kotlin companion object when the
    /// type itself declares none (companion members compile into the enclosing class).
    fn direct_or_companion(
        &self,
        members: &[Member],
        accept: impl Fn(&Member) -> bool,
    ) -> Vec<Member> {
        let direct = members
            .iter()
            .filter(|member| accept(member))
            .cloned()
            .collect::<Vec<_>>();
        if !direct.is_empty() {
            return direct;
        }
        members
            .iter()
            .filter_map(|member| match &member.kind {
                MemberKind::Type(declaration) if declaration.is_companion => {
                    Some(self.members_of(declaration, member))
                }
                _ => None,
            })
            .flatten()
            .filter(|member| accept(member))
            .collect()
    }

    fn unresolved_members(&self, members: &[&Member]) -> SliceOutcome {
        if members.is_empty() {
            return SliceOutcome::not_found();
        }
        SliceOutcome::unresolved(
            UnresolvedDeclarationReason::Ambiguous,
            self.member_lines(members),
        )
    }

    fn member_lines(&self, members: &[&Member]) -> Vec<usize> {
        let mut lines = members
            .iter()
            .map(|member| self.tokens[member.start].line + 1)
            .collect::<Vec<_>>();
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    fn slice(&self, start: usize, end: usize) -> SourceSlice {
        let start_line = self.tokens[start].line;
        let end_line = self.tokens[end].end_line.max(start_line);
        let source = self
            .source
            .lines()
            .skip(start_line)
            .take(end_line - start_line + 1)
            .collect::<Vec<_>>()
            .join("\n");
        SourceSlice {
            start_line: start_line + 1,
            end_line: end_line + 1,
            source,
        }
    }

    /// Splits the contents of the bracket pair `open`..`close` at top-level commas.
    fn split_list(&self, open: usize, close: usize) -> Vec<(usize, usize)> {
        let mut parts = Vec::new();
        let mut start = open + 1;
        let mut angle_depth = 0usize;
        let mut index = start;
        while index < close {
            if self.is_punct(index, "<") {
                angle_depth += 1;
            } else if self.is_punct(index, ">") {
                angle_depth = angle_depth.saturating_sub(1);
            } else if angle_depth == 0 && self.is_punct(index, ",") {
                parts.push((start, index));
                start = index + 1;
            }
            index = self.group_end(index, close) + 1;
        }
        parts.push((start, close));
        parts.retain(|(start, end)| start < end);
        parts
    }

    fn text(&self, index: usize) -> Option<&'a str> {
        self.tokens
            .get(index)
            .map(|token| &self.source[token.start..token.end])
    }

    fn word(&self, index: usize) -> Option<&'a str> {
        self.tokens
            .get(index)
            .filter(|token| token.kind == TokenKind::Identifier)
            .and_then(|_| self.text(index))
    }

    /// An identifier with Kotlin backticks removed.
    fn identifier(&self, index: usize) -> Option<&'a str> {
        self.word(index).map(|word| word.trim_matches('`'))
    }

    fn is_punct(&self, index: usize, value: &str) -> bool {
        self.tokens
            .get(index)
            .is_some_and(|token| token.kind == TokenKind::Punctuation)
            && self.text(index) == Some(value)
    }

    fn is_closer(&self, index: usize) -> bool {
        [")", "]", "}"]
            .iter()
            .any(|value| self.is_punct(index, value))
    }

    fn closing(&self, index: usize) -> Option<usize> {
        self.matching.get(index).copied().flatten()
    }

    /// Index of the closer of a bracket opened at `index`, or `index` for other tokens.
    ///
    /// An unclosed bracket extends to the last token before `end`.
    fn group_end(&self, index: usize, end: usize) -> usize {
        if !["(", "[", "{"]
            .iter()
            .any(|value| self.is_punct(index, value))
        {
            return index;
        }
        self.closing(index)
            .filter(|close| *close > index && *close < end)
            .unwrap_or(end.saturating_sub(1).max(index))
    }

    /// Index of the `>` that closes the `<` at `open`, or the last token before `end`.
    fn angle_end(&self, open: usize, end: usize) -> usize {
        let mut depth = 0usize;
        let mut index = open;
        while index < end {
            if self.is_punct(index, "<") {
                depth += 1;
            } else if self.is_punct(index, ">") {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return index;
                }
            } else if self.is_punct(index, "{") || self.is_punct(index, ";") {
                return index.saturating_sub(1).max(open);
            }
            index = self.group_end(index, end) + 1;
        }
        end.saturating_sub(1).max(open)
    }
}

/// Simple names of the binary class name, from the outermost type; `None` for local classes.
fn nested_type_path(class_name: &str) -> Option<Vec<&str>> {
    let simple = class_name.rsplit('.').next()?;
    let path = simple.split('$').collect::<Vec<_>>();
    path.iter()
        .all(|segment| {
            segment
                .chars()
                .next()
                .is_some_and(|first| !first.is_ascii_digit())
        })
        .then_some(path)
}

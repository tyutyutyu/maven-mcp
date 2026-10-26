//! Erasure of source parameter types for matching them against classfile descriptors.

use super::{
    FunctionDeclaration, Member, MemberKind, Owners, SourceLanguage, SourceOutline,
    TypeDeclaration, TypeVariables, members::KOTLIN_MODIFIERS,
};

/// Nesting limit of parenthesized and array element types in one Kotlin type.
const MAX_TYPE_NESTING: usize = 32;
/// Limit of type variable bounds followed while erasing a type variable.
const MAX_TYPE_VARIABLE_DEPTH: usize = 8;

/// An erased JVM parameter type identified by its simple name and array dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ErasedType {
    name: String,
    dimensions: usize,
}

impl ErasedType {
    fn new(name: &str, dimensions: usize) -> Self {
        Self {
            name: name.to_owned(),
            dimensions,
        }
    }
}

/// How a source function relates to the parameters of a classfile descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DescriptorMatch {
    /// The erased source parameters equal the descriptor parameters.
    Proven,
    /// The parameter list was cut off by truncation, so it can be neither proven nor ruled out.
    Possible,
    /// The erased source parameters differ from the descriptor parameters.
    Excluded,
}

pub(super) fn descriptor_parameters(descriptor: &str) -> Option<Vec<ErasedType>> {
    let (parameters, _) = descriptor.strip_prefix('(')?.split_once(')')?;
    let mut result = Vec::new();
    let mut characters = parameters.chars();
    let mut dimensions = 0;
    while let Some(character) = characters.next() {
        let name = match character {
            '[' => {
                dimensions += 1;
                continue;
            }
            'L' => {
                let binary = characters
                    .by_ref()
                    .take_while(|value| *value != ';')
                    .collect::<String>();
                binary
                    .rsplit('/')
                    .next()
                    .and_then(|name| name.rsplit('$').next())
                    .unwrap_or_default()
                    .to_owned()
            }
            other => primitive_name(other)?.to_owned(),
        };
        result.push(ErasedType { name, dimensions });
        dimensions = 0;
    }
    (dimensions == 0).then_some(result)
}

fn primitive_name(descriptor: char) -> Option<&'static str> {
    Some(match descriptor {
        'B' => "byte",
        'C' => "char",
        'D' => "double",
        'F' => "float",
        'I' => "int",
        'J' => "long",
        'S' => "short",
        'Z' => "boolean",
        _ => return None,
    })
}

/// Maps a Kotlin type name to the simple name of its erased JVM type.
fn kotlin_erased_name(name: &str, boxed: bool) -> ErasedType {
    let primitive = match name {
        "Int" => Some(("int", "Integer")),
        "Long" => Some(("long", "Long")),
        "Short" => Some(("short", "Short")),
        "Byte" => Some(("byte", "Byte")),
        "Char" => Some(("char", "Character")),
        "Boolean" => Some(("boolean", "Boolean")),
        "Float" => Some(("float", "Float")),
        "Double" => Some(("double", "Double")),
        _ => None,
    };
    if let Some((primitive, boxed_name)) = primitive {
        return ErasedType::new(if boxed { boxed_name } else { primitive }, 0);
    }
    if let Some(element) = name.strip_suffix("Array") {
        let element = kotlin_erased_name(element, false);
        if element.name.chars().next().is_some_and(char::is_lowercase) {
            return ErasedType::new(&element.name, 1);
        }
    }
    let mapped = match name {
        "Any" => "Object",
        "Nothing" => "Void",
        "MutableList" => "List",
        "MutableSet" => "Set",
        "MutableMap" => "Map",
        "MutableCollection" => "Collection",
        "MutableIterable" => "Iterable",
        "MutableIterator" => "Iterator",
        "MutableListIterator" => "ListIterator",
        "MutableEntry" => "Entry",
        other => other,
    };
    ErasedType::new(mapped, 0)
}

impl SourceOutline<'_> {
    pub(super) fn function_matches(
        &self,
        member: &Member,
        owners: &Owners,
        expected: &[ErasedType],
    ) -> DescriptorMatch {
        let MemberKind::Function(function) = &member.kind else {
            return DescriptorMatch::Excluded;
        };
        if !self.is_punct(function.parameters.1, ")") {
            return DescriptorMatch::Possible;
        }
        if self.erased_parameters_match(function, owners, expected) {
            DescriptorMatch::Proven
        } else {
            DescriptorMatch::Excluded
        }
    }

    fn erased_parameters_match(
        &self,
        function: &FunctionDeclaration,
        owners: &Owners,
        expected: &[ErasedType],
    ) -> bool {
        let mut variables = TypeVariables::new();
        for (owner, _) in owners {
            self.collect_type_variables(owner.type_parameters, &mut variables);
        }
        self.collect_type_variables(function.type_parameters, &mut variables);
        let Some(actual) = self.erased_signature(function, &variables) else {
            return false;
        };
        if actual == expected {
            return true;
        }
        let Some((owner, _)) = owners.last() else {
            return false;
        };
        let enclosing = owners
            .len()
            .checked_sub(2)
            .map(|index| owners[index].0.name.as_str());
        function.is_constructor && synthetic_constructor_prefix(owner, enclosing, &actual, expected)
    }

    fn erased_signature(
        &self,
        function: &FunctionDeclaration,
        variables: &TypeVariables,
    ) -> Option<Vec<ErasedType>> {
        let mut signature = Vec::new();
        if let Some((start, end)) = function.receiver {
            signature.push(self.erase(self.kotlin_type(start, end, false, 0)?, variables, 0));
        }
        let (open, close) = function.parameters;
        for (start, end) in self.split_list(open, close) {
            let parameter = match self.language {
                SourceLanguage::Java => self.java_parameter(start, end)?,
                SourceLanguage::Kotlin => Some(self.kotlin_parameter(start, end)?),
            };
            if let Some(parameter) = parameter {
                signature.push(self.erase(parameter, variables, 0));
            }
        }
        if function.is_suspend {
            signature.push(ErasedType::new("Continuation", 0));
        }
        Some(signature)
    }

    pub(super) fn java_parameter_name_index(&self, start: usize, end: usize) -> Option<usize> {
        let (_, index) = self.java_type(self.skip_modifiers(start, end).0, end)?;
        (index < end && self.identifier(index).is_some()).then_some(index)
    }

    /// Erased type of one Java parameter; `Some(None)` for a receiver parameter.
    fn java_parameter(&self, start: usize, end: usize) -> Option<Option<ErasedType>> {
        let (index, _) = self.skip_modifiers(start, end);
        let (mut erased, index) = self.java_type(index, end)?;
        if self.word(index) == Some("this") || self.word(index + 2) == Some("this") {
            return Some(None);
        }
        let mut cursor = index + 1;
        while cursor + 1 < end && self.is_punct(cursor, "[") && self.is_punct(cursor + 1, "]") {
            erased.dimensions += 1;
            cursor += 2;
        }
        Some(Some(erased))
    }

    /// Parses a Java type at `index`; returns it with the index of the following token.
    fn java_type(&self, mut index: usize, end: usize) -> Option<(ErasedType, usize)> {
        let mut name;
        loop {
            index = self.skip_annotations(index, end);
            name = self.identifier(index).filter(|_| index < end)?;
            index += 1;
            if self.is_punct(index, "<") {
                index = self.angle_end(index, end) + 1;
            }
            if self.is_punct(index, ".") && self.identifier(index + 1).is_some() {
                index += 1;
                continue;
            }
            break;
        }
        let mut dimensions = 0;
        loop {
            index = self.skip_annotations(index, end);
            if self.is_punct(index, "[") && self.is_punct(index + 1, "]") {
                dimensions += 1;
                index += 2;
            } else {
                break;
            }
        }
        if (0..3).all(|offset| self.is_punct(index + offset, ".")) {
            dimensions += 1;
            index += 3;
        }
        Some((ErasedType::new(name, dimensions), index))
    }

    fn kotlin_parameter(&self, start: usize, end: usize) -> Option<ErasedType> {
        let mut index = self.skip_annotations(start, end);
        let mut is_vararg = false;
        while let Some(word) = self.word(index) {
            let is_modifier = matches!(word, "val" | "var") || KOTLIN_MODIFIERS.contains(&word);
            if !is_modifier || self.is_punct(index + 1, ":") {
                break;
            }
            is_vararg |= word == "vararg";
            index = self.skip_annotations(index + 1, end);
        }
        if self.identifier(index).is_none() || !self.is_punct(index + 1, ":") {
            return None;
        }
        let type_start = index + 2;
        let mut type_end = type_start;
        while type_end < end && !self.is_punct(type_end, "=") {
            type_end = self.group_end(type_end, end) + 1;
        }
        let mut erased = self.kotlin_type(type_start, type_end.min(end), false, 0)?;
        if is_vararg {
            erased.dimensions += 1;
        }
        Some(erased)
    }

    /// Erases the Kotlin type in the half-open token range `start..end`.
    fn kotlin_type(
        &self,
        start: usize,
        end: usize,
        boxed: bool,
        depth: usize,
    ) -> Option<ErasedType> {
        if depth >= MAX_TYPE_NESTING {
            return None;
        }
        let mut index = self.skip_annotations(start, end);
        let is_suspend = self.word(index) == Some("suspend");
        if is_suspend {
            index += 1;
        }
        let nullable = end > index && self.is_punct(end - 1, "?");
        if let Some(arity) = self.kotlin_function_type_arity(index, end) {
            return Some(ErasedType {
                name: format!("Function{}", arity + usize::from(is_suspend)),
                dimensions: 0,
            });
        }
        if self.is_punct(index, "(") {
            let close = self.group_end(index, end);
            return self.kotlin_type(index + 1, close, boxed || nullable, depth + 1);
        }
        let mut name = self.identifier(index).filter(|_| index < end)?;
        let mut arguments = None;
        loop {
            index += 1;
            if self.is_punct(index, "<") {
                let close = self.angle_end(index, end);
                arguments = Some((index, close));
                index = close + 1;
            }
            match self.identifier(index + 1) {
                Some(segment) if self.is_punct(index, ".") && index + 1 < end => {
                    name = segment;
                    arguments = None;
                    index += 1;
                }
                _ => break,
            }
        }
        if name == "Array" {
            return Some(self.kotlin_array_type(arguments, depth));
        }
        Some(kotlin_erased_name(name, boxed || nullable))
    }

    /// Erases `Array<T>` from its type arguments, `Array<*>` as an `Object` array.
    fn kotlin_array_type(&self, arguments: Option<(usize, usize)>, depth: usize) -> ErasedType {
        let element = arguments
            .and_then(|(open, close)| self.split_list(open, close).into_iter().next())
            .and_then(|(start, end)| {
                let start = match self.word(start) {
                    Some("out" | "in") => start + 1,
                    _ => start,
                };
                self.kotlin_type(start, end, true, depth + 1)
            })
            .unwrap_or_else(|| ErasedType::new("Object", 0));
        ErasedType {
            name: element.name,
            dimensions: element.dimensions + 1,
        }
    }

    /// Parameter count of a Kotlin function type, a receiver counted as one parameter.
    fn kotlin_function_type_arity(&self, start: usize, end: usize) -> Option<usize> {
        let mut index = start;
        while index < end && !self.is_punct(index, "->") {
            index = self.group_end(index, end) + 1;
        }
        let close = index.checked_sub(1)?;
        if index >= end || !self.is_punct(close, ")") {
            return None;
        }
        let open = self
            .closing(close)
            .filter(|open| *open >= start && *open < close)?;
        let receiver = usize::from(open > start);
        Some(self.split_list(open, close).len() + receiver)
    }

    fn collect_type_variables(
        &self,
        parameters: Option<(usize, usize)>,
        variables: &mut TypeVariables,
    ) {
        let Some((open, close)) = parameters else {
            return;
        };
        for (start, end) in self.split_list(open, close) {
            let mut index = self.skip_annotations(start, end);
            if matches!(self.word(index), Some("reified" | "in" | "out")) {
                index += 1;
            }
            let Some(name) = self.identifier(index) else {
                continue;
            };
            let has_bound =
                self.word(index + 1) == Some("extends") || self.is_punct(index + 1, ":");
            let bound_start = index + 2;
            let bound = (has_bound && bound_start < end).then(|| {
                let mut bound_end = bound_start;
                while bound_end < end && !self.is_punct(bound_end, "&") {
                    bound_end = self.group_end(bound_end, end) + 1;
                }
                (bound_start, bound_end.min(end))
            });
            variables.insert(name.to_owned(), bound);
        }
    }

    fn erase(&self, erased: ErasedType, variables: &TypeVariables, depth: usize) -> ErasedType {
        let Some(bound) = variables.get(&erased.name) else {
            return erased;
        };
        if depth >= MAX_TYPE_VARIABLE_DEPTH {
            return ErasedType::new("Object", erased.dimensions);
        }
        let base = bound
            .and_then(|(start, end)| match self.language {
                SourceLanguage::Java => self.java_type(start, end).map(|(erased, _)| erased),
                SourceLanguage::Kotlin => self.kotlin_type(start, end, true, 0),
            })
            .map_or_else(
                || ErasedType::new("Object", 0),
                |base| self.erase(base, variables, depth + 1),
            );
        ErasedType {
            name: base.name,
            dimensions: base.dimensions + erased.dimensions,
        }
    }
}

/// Accepts the synthetic leading constructor parameters javac and kotlinc add for inner
/// classes (the enclosing instance) and enums (name and ordinal).
fn synthetic_constructor_prefix(
    owner: &TypeDeclaration,
    enclosing: Option<&str>,
    actual: &[ErasedType],
    expected: &[ErasedType],
) -> bool {
    let inner = enclosing.is_some_and(|enclosing| {
        expected.len() == actual.len() + 1
            && expected[0] == ErasedType::new(enclosing, 0)
            && expected[1..] == *actual
    });
    let enumeration = owner.is_enum
        && expected.len() == actual.len() + 2
        && expected[0] == ErasedType::new("String", 0)
        && expected[1] == ErasedType::new("int", 0)
        && expected[2..] == *actual;
    inner || enumeration
}

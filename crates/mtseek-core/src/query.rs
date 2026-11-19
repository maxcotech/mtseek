/// How the pattern text is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PatternKind {
    /// Rust `regex` syntax, as in ripgrep (the default).
    #[default]
    Regex,
    /// The pattern is matched byte-for-byte (`rg --fixed-strings`).
    Literal,
}

/// How letter case is handled when matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMode {
    /// Case must match exactly (ripgrep's default).
    #[default]
    Sensitive,
    /// Case is ignored (`rg --ignore-case`).
    Insensitive,
    /// Case is ignored unless the pattern contains an uppercase letter (`rg --smart-case`).
    Smart,
}

/// A content search request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// The pattern text as the user typed it.
    pub pattern: String,
    /// Literal or regex.
    pub kind: PatternKind,
    /// Case handling.
    pub case: CaseMode,
}

impl Query {
    /// A case-sensitive regex query, matching ripgrep's defaults.
    pub fn regex(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            kind: PatternKind::Regex,
            case: CaseMode::Sensitive,
        }
    }

    /// A case-sensitive literal query (`rg -F`).
    pub fn literal(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            kind: PatternKind::Literal,
            case: CaseMode::Sensitive,
        }
    }

    /// Returns the query with a different case mode.
    #[must_use]
    pub fn with_case(mut self, case: CaseMode) -> Self {
        self.case = case;
        self
    }
}

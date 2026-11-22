use std::path::PathBuf;

/// Which files a search covers.
///
/// The contract (see `docs/adr/0005-ripgrep-compatible-contract.md`): for the same roots
/// and filter, mtseek searches exactly the files `rg --files` would list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// Directories or files to search. Paths are reported relative to how they are given here.
    pub roots: Vec<PathBuf>,
    /// Which files under the roots are included.
    pub filter: Filter,
}

/// File filters, mirroring ripgrep's flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // each flag maps 1:1 to a ripgrep option
pub struct Filter {
    /// Honour `.gitignore`, `.ignore`, `.rgignore` and git excludes (`rg` default; `--no-ignore` turns it off).
    pub respect_ignore_files: bool,
    /// Include hidden files and directories (`rg --hidden`).
    pub include_hidden: bool,
    /// Search binary files instead of skipping them (`rg --binary`).
    pub include_binary: bool,
    /// Follow symbolic links and junctions (`rg --follow`).
    pub follow_links: bool,
}

impl Default for Filter {
    fn default() -> Self {
        Self::unrestricted(0)
    }
}

impl Filter {
    /// The filter for ripgrep's `-u` repetition count.
    ///
    /// * `0`: defaults (ignore files honoured, hidden and binary skipped)
    /// * `1` (`-u`): ignore files not honoured
    /// * `2` (`-uu`): also include hidden files
    /// * `3+` (`-uuu`): also search binary files
    pub fn unrestricted(level: u8) -> Self {
        Self {
            respect_ignore_files: level < 1,
            include_hidden: level >= 2,
            include_binary: level >= 3,
            follow_links: false,
        }
    }
}

/// Glob and file-type rules on paths (`rg -g`, `--iglob`, `-t`, `-T`).
///
/// Unlike [`Filter`], these can list files the filter alone wouldn't: a file matching a glob
/// is searched even if it is ignored or hidden, and a file of a selected type even if it is
/// hidden (ripgrep's rules, which `mtseek-walk` applies).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Select {
    /// `-g` globs, in command-line order. `!` in front excludes.
    pub globs: Vec<String>,
    /// `--iglob` globs: like `globs`, matched without regard to case, and after them.
    pub iglobs: Vec<String>,
    /// `-t` and `-T`, in command-line order.
    pub types: Vec<TypeRule>,
}

/// One `-t` (select) or `-T` (negate) of a file type by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRule {
    /// The type's name, as in `rg --type-list`.
    pub name: String,
    /// `-T`: leave files of this type out.
    pub negated: bool,
}

impl Select {
    /// Whether there are no rules.
    pub fn is_empty(&self) -> bool {
        self.globs.is_empty() && self.iglobs.is_empty() && self.types.is_empty()
    }
}

impl Scope {
    /// A scope over the given roots with ripgrep's default filters.
    pub fn new(roots: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self {
            roots: roots.into_iter().map(Into::into).collect(),
            filter: Filter::default(),
        }
    }

    /// Returns the scope with a different filter.
    #[must_use]
    pub fn with_filter(mut self, filter: Filter) -> Self {
        self.filter = filter;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrestricted_levels_match_ripgrep() {
        let f0 = Filter::unrestricted(0);
        assert!(f0.respect_ignore_files && !f0.include_hidden && !f0.include_binary);
        let f1 = Filter::unrestricted(1);
        assert!(!f1.respect_ignore_files && !f1.include_hidden && !f1.include_binary);
        let f2 = Filter::unrestricted(2);
        assert!(!f2.respect_ignore_files && f2.include_hidden && !f2.include_binary);
        let f3 = Filter::unrestricted(3);
        assert!(!f3.respect_ignore_files && f3.include_hidden && f3.include_binary);
    }
}

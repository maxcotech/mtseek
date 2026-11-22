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
    /// Which ignore files are read, when it isn't all or none of them.
    pub ignore_files: IgnoreFiles,
}

/// Finer control over ignore files than [`Filter::respect_ignore_files`], with ripgrep's
/// flags. The default reads what the filter says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)] // each flag maps 1:1 to a ripgrep option
pub struct IgnoreFiles {
    /// Skip `.gitignore`, `.git/info/exclude` and git's global excludes (`--no-ignore-vcs`).
    pub no_vcs: bool,
    /// Skip `.ignore` and `.rgignore` (`--no-ignore-dot`).
    pub no_dot: bool,
    /// Skip ignore files in folders above the search root (`--no-ignore-parent`).
    pub no_parent: bool,
    /// Skip git's global excludes file (`--no-ignore-global`).
    pub no_global: bool,
    /// Skip `.git/info/exclude` (`--no-ignore-exclude`).
    pub no_exclude: bool,
    /// More ignore files in gitignore syntax (`--ignore-file`), read even when the filter
    /// doesn't respect ignore files, as ripgrep does. Relative paths are found from the
    /// working directory.
    pub extra: Vec<PathBuf>,
}

impl IgnoreFiles {
    /// Whether a walk with `filter` and these settings reads exactly the ignore files the
    /// filter alone reads: the walks a content index records (ADR 0012).
    pub fn is_standard_for(&self, filter: &Filter) -> bool {
        let finer =
            self.no_vcs || self.no_dot || self.no_parent || self.no_global || self.no_exclude;
        self.extra.is_empty() && !(filter.respect_ignore_files && finer)
    }
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
        self.globs.is_empty()
            && self.iglobs.is_empty()
            && self.types.is_empty()
            && self.ignore_files == IgnoreFiles::default()
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
    use std::path::PathBuf;

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

    /// The `--no-ignore-*` flags change nothing once `-u` reads no ignore files, but
    /// `--ignore-file` applies even then (ripgrep 15).
    #[test]
    fn finer_ignore_settings_leave_the_standard_walks_only_where_they_change_them() {
        let default = Filter::default();
        let no_ignore = Filter::unrestricted(1);
        assert!(IgnoreFiles::default().is_standard_for(&default));
        let vcs = IgnoreFiles {
            no_vcs: true,
            ..IgnoreFiles::default()
        };
        assert!(!vcs.is_standard_for(&default));
        assert!(vcs.is_standard_for(&no_ignore));
        let extra = IgnoreFiles {
            extra: vec![PathBuf::from("x.ignore")],
            ..IgnoreFiles::default()
        };
        assert!(!extra.is_standard_for(&no_ignore));
    }
}

// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The warning about an installed agent skill that is older than this
//! antiq (spec section "skill"): where installed copies are looked for,
//! how old a copy is, and the text of the warning. `antiq skill` makes one
//! call, [`outdated_copies`]; everything else is here.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use antiq::report::{Warning, WarningKind};

// =========================================================================
// Where installed copies are looked for
// =========================================================================

/// Skills directories of a project, relative to the current directory and
/// to every directory above it up to the git root. Each entry is a
/// location the documentation of an agent names.
const PROJECT_DIRS: &[&str] = &[
    // The shared directory several agents read: Codex
    // (https://learn.chatgpt.com/docs/build-skills), Gemini CLI
    // (https://geminicli.com/docs/cli/skills/), GitHub Copilot
    // (https://docs.github.com/en/copilot/concepts/customizations/agent-skills)
    // and Cursor (https://cursor.com/docs/skills).
    ".agents/skills",
    // Claude Code: https://code.claude.com/docs/en/skills
    ".claude/skills",
    // Gemini CLI: https://geminicli.com/docs/cli/skills/
    ".gemini/skills",
    // GitHub Copilot:
    // https://docs.github.com/en/copilot/concepts/customizations/agent-skills
    ".github/skills",
    // Cursor: https://cursor.com/docs/skills
    ".cursor/skills",
    // Cursor loads the Codex directory too: https://cursor.com/docs/skills
    ".codex/skills",
];

/// How much of a `SKILL.md` is read. The frontmatter is at the top, so
/// whatever lies beyond this is never needed.
const PREFIX: usize = 16 * 1024;

/// A global skills directory.
struct GlobalDir {
    /// Relative to the home directory.
    home_relative: &'static str,
    /// A variable that moves the agent's directory elsewhere.
    moved_by: Option<Override>,
}

/// An environment variable that names a directory an agent uses in place
/// of its default one. The skills are then in `$variable/<subpath>`, and
/// that directory is searched right after the default one.
struct Override {
    variable: &'static str,
    subpath: &'static str,
}

/// Global skills directories, in the order a copy found in several is
/// named by the first.
const GLOBAL_DIRS: &[GlobalDir] = &[
    GlobalDir {
        // The shared directory: Codex
        // (https://learn.chatgpt.com/docs/build-skills), Gemini CLI
        // (https://geminicli.com/docs/cli/skills/), GitHub Copilot and
        // Cursor (see above).
        home_relative: ".agents/skills",
        // Gemini CLI keeps this directory and `.gemini` below the home
        // that `GEMINI_CLI_HOME` names in place of the user's home:
        // `docs/reference/configuration.md:2696` and
        // `packages/core/src/config/storage.ts:54-68,142-148` in
        // https://github.com/google-gemini/gemini-cli at b633c07.
        moved_by: Some(Override {
            variable: "GEMINI_CLI_HOME",
            subpath: ".agents/skills",
        }),
    },
    GlobalDir {
        // Claude Code: https://code.claude.com/docs/en/skills
        home_relative: ".claude/skills",
        // It keeps its configuration in `CLAUDE_CONFIG_DIR` in place of
        // `~/.claude`: https://code.claude.com/docs/en/env-vars and
        // https://code.claude.com/docs/en/claude-directory
        moved_by: Some(Override {
            variable: "CLAUDE_CONFIG_DIR",
            subpath: "skills",
        }),
    },
    GlobalDir {
        // Gemini CLI: https://geminicli.com/docs/cli/skills/
        home_relative: ".gemini/skills",
        // `GEMINI_CLI_HOME`, see the shared directory above.
        moved_by: Some(Override {
            variable: "GEMINI_CLI_HOME",
            subpath: ".gemini/skills",
        }),
    },
    GlobalDir {
        // GitHub Copilot:
        // https://docs.github.com/en/copilot/concepts/customizations/agent-skills
        home_relative: ".copilot/skills",
        // `COPILOT_HOME` replaces `~/.copilot`:
        // https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-config-dir-reference
        moved_by: Some(Override {
            variable: "COPILOT_HOME",
            subpath: "skills",
        }),
    },
    GlobalDir {
        // Cursor: https://cursor.com/docs/skills
        home_relative: ".cursor/skills",
        moved_by: None,
    },
    GlobalDir {
        // Cursor loads the Codex directory too:
        // https://cursor.com/docs/skills
        home_relative: ".codex/skills",
        // Codex keeps its state in `CODEX_HOME` in place of `~/.codex`
        // (https://developers.openai.com/codex/config-advanced), and its
        // skills in `skills` below it: `codex-rs/exec-server/src/discoverV2/capability_locations.rs:73`
        // in https://github.com/openai/codex at a4b4082.
        moved_by: Some(Override {
            variable: "CODEX_HOME",
            subpath: "skills",
        }),
    },
];

/// The variables of the table of global directories that move one, each
/// once. A test that must not find the skills of whoever runs it removes
/// them all; `make_hermetic` in `tests/support/cmd.rs` names the same.
#[cfg(test)]
fn override_variables() -> impl Iterator<Item = &'static str> {
    let mut seen = Vec::new();
    for moved in GLOBAL_DIRS.iter().filter_map(|dir| dir.moved_by.as_ref()) {
        if !seen.contains(&moved.variable) {
            seen.push(moved.variable);
        }
    }
    seen.into_iter()
}

/// What a search needs to know, so tests can give it directories of their
/// own.
struct Search<'a> {
    cwd: &'a Path,
    home: Option<&'a Path>,
    /// The global directories, [`GLOBAL_DIRS`].
    global: &'a [GlobalDir],
    /// The value of an environment variable by its name.
    variable: &'a dyn Fn(&str) -> Option<OsString>,
    /// The directory the walk up from `cwd` ends in, the git root. None
    /// searches `cwd` alone.
    stop: Option<&'a Path>,
    /// The version of the antiq that runs.
    running: &'a str,
}

/// The warning about the installed copies of the skill that are older than
/// this antiq, searched from `start`, the directory the run starts in, up
/// to its git root, and in the global directories of `HOME` and the
/// variables that move them.
pub(super) fn outdated_copies(start: &Path) -> Option<Warning> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let stop = git_root(start, None);
    let outdated = check(&Search {
        cwd: start,
        home: home.as_deref(),
        global: GLOBAL_DIRS,
        variable: &|name| std::env::var_os(name),
        stop: stop.as_deref(),
        running: env!("CARGO_PKG_VERSION"),
    })?;
    Some(Warning::of(WarningKind::SkillOutdated {
        message: outdated.message,
        fix: outdated.fix,
    }))
}

/// The directory a variable names, none when it names none.
///
/// A relative value has no stable meaning for antiq, which runs from
/// another directory than the agent may have, and neither has an empty
/// one or one that starts with a `~` that no shell expanded.
fn override_dir(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|dir| dir.is_absolute())
}

/// The nearest directory from `cwd` up that holds a `.git`, a directory in
/// a repository and a file in a linked worktree. The search does not go
/// above `ceiling`.
fn git_root(cwd: &Path, ceiling: Option<&Path>) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        // `symlink_metadata` also counts a `.git` that is a dangling
        // link: the directory is still where a repository was meant to
        // be.
        if std::fs::symlink_metadata(dir.join(".git")).is_ok() {
            return Some(dir.to_owned());
        }
        if Some(dir) == ceiling {
            break;
        }
    }
    None
}

/// The warning's texts, none when no installed copy is older than
/// `search.running`.
fn check(search: &Search<'_>) -> Option<Outdated> {
    let running = parse_version(search.running)?;
    let mut old = Vec::new();
    // Agents share one copy through symlinks, so a copy counts once, by
    // the real path of its file, under the first directory that reaches it.
    let mut seen = HashSet::new();
    let mut consider = |shown: Shown, dir: PathBuf| {
        let file = dir.join("SKILL.md");
        let (Some(from), Ok(real)) = (read_stamp(&file), file.canonicalize()) else {
            return;
        };
        if seen.insert(real) && is_older(from, running) {
            old.push(OldCopy { shown, from });
        }
    };
    let canonical_home = search.home.and_then(|home| home.canonicalize().ok());
    // A directory below the home directory is named from it, as given
    // and as the system resolves it (the current directory is the
    // resolved one), and any other by its full path.
    let shown_below_home = |dir: PathBuf| {
        let under_home = [search.home, canonical_home.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|home| dir.strip_prefix(home).ok().map(Path::to_owned));
        match under_home {
            Some(rest) => Shown::Home(rest),
            None => Shown::Absolute(dir),
        }
    };
    for (level, root) in project_roots(search.cwd, search.stop)
        .into_iter()
        .enumerate()
    {
        for entry in PROJECT_DIRS {
            // Where the copy is, as the warning names it: the directory of
            // the agent for the level the agent works in, otherwise by
            // the home directory when the copy is below it, and by its
            // full path when it is not.
            let shown = if level == 0 {
                Shown::Relative(entry.into())
            } else {
                shown_below_home(root.join(entry))
            };
            consider(shown, root.join(entry).join("antiq"));
        }
    }
    for dir in search.global {
        if let Some(home) = search.home {
            consider(
                Shown::Home(dir.home_relative.into()),
                home.join(dir.home_relative).join("antiq"),
            );
        }
        // The default directory stays searched: it is not documented
        // whether the agent still reads it once the variable is set.
        if let Some(moved) = &dir.moved_by
            && let Some(base) = override_dir((search.variable)(moved.variable))
        {
            let skills = base.join(moved.subpath);
            consider(shown_below_home(skills.clone()), skills.join("antiq"));
        }
    }
    (!old.is_empty()).then(|| hint_text(&old, search.running))
}

/// The stamp of the `SKILL.md` at `file`. None when the file cannot be read
/// as a regular file, or its stamp is no version: such a copy is not
/// reported, since a warning about the check helps nobody.
fn read_stamp(file: &Path) -> Option<Stamp> {
    let mut prefix = Vec::new();
    // `open` does not wait for a writer on a FIFO.
    open_regular(file)
        .ok()?
        .take(PREFIX as u64)
        .read_to_end(&mut prefix)
        .ok()?;
    // Whatever follows the frontmatter is not read, so a byte that is no
    // UTF-8, or a character cut at the end of the prefix, must not hide
    // the copy.
    stamp_of(&String::from_utf8_lossy(&prefix))
}

/// Opens `file` for reading and refuses anything but a regular file. The
/// open does not wait, so a FIFO in place of a `SKILL.md` cannot stall the
/// run, and a terminal device does not become the controlling terminal.
/// Once the file is known to be regular, its reads block as usual again.
/// A symlink is followed, since installers link the skill directories of
/// several agents to one copy.
fn open_regular(file: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, fcntl_getfl, fcntl_setfl, open};

    let opened = File::from(open(
        file,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty(),
    )?);
    if !opened.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    fcntl_setfl(&opened, fcntl_getfl(&opened)? - OFlags::NONBLOCK)?;
    Ok(opened)
}

/// `cwd` and each directory above it up to `stop`, nearest first. `cwd`
/// alone when there is no `stop`, or `stop` is not above it: without a
/// repository there is no root to end the walk at.
fn project_roots(cwd: &Path, stop: Option<&Path>) -> Vec<PathBuf> {
    let chain: Vec<&Path> = cwd.ancestors().collect();
    let end = stop
        .and_then(|stop| chain.iter().position(|dir| *dir == stop))
        .unwrap_or(0);
    chain[..=end].iter().map(|dir| dir.to_path_buf()).collect()
}

/// A copy without a stamp is older than any antiq.
fn is_older(copy: Stamp, running: Version) -> bool {
    match copy {
        Stamp::Unstamped => true,
        Stamp::Version(version) => version < running,
    }
}

// =========================================================================
// Versions
// =========================================================================

/// `antiq-version` of a copy: three numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

/// What a copy says about the antiq it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stamp {
    /// No `antiq-version`: a copy written by hand or by another tool, as
    /// every skill antiq ships carries one.
    Unstamped,
    Version(Version),
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// `MAJOR.MINOR.PATCH`, numbers only. A pre-release or build suffix after
/// the patch number does not count.
fn parse_version(text: &str) -> Option<Version> {
    let core = text.split(['-', '+']).next()?;
    // `u64::from_str` accepts a leading `+`, which is no digit.
    let number = |part: &str| {
        part.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let mut parts = core.split('.');
    let version = Version(
        number(parts.next()?)?,
        number(parts.next()?)?,
        number(parts.next()?)?,
    );
    parts.next().is_none().then_some(version)
}

/// The stamp in the frontmatter of the text of a `SKILL.md`. None when the
/// frontmatter has an `antiq-version` that is no version.
///
/// The one key is read by hand: a YAML parser would be a dependency for
/// a single line, and the line is the same wherever it is nested.
fn stamp_of(text: &str) -> Option<Stamp> {
    let Some(value) = frontmatter(text).into_iter().find_map(|line| {
        line.trim_start()
            .strip_prefix("antiq-version:")
            .map(str::trim)
    }) else {
        return Some(Stamp::Unstamped);
    };
    let unquoted = ['"', '\'']
        .iter()
        .find_map(|quote| value.strip_prefix(*quote)?.strip_suffix(*quote))
        .unwrap_or(value);
    parse_version(unquoted).map(Stamp::Version)
}

/// The lines between the two `---` lines that open a text. A text that
/// does not open with one, or never closes it, has none.
fn frontmatter(text: &str) -> Vec<&str> {
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return Vec::new();
    }
    let mut inside = Vec::new();
    for line in lines {
        if line == "---" {
            return inside;
        }
        inside.push(line);
    }
    Vec::new()
}

// =========================================================================
// The text of the warning
// =========================================================================

/// Where a copy is shown to be, which is also where `antiq skill --install`
/// takes it.
#[derive(Debug, PartialEq, Eq)]
enum Shown {
    /// Below the current directory: the skills directory as the table
    /// names it.
    Relative(PathBuf),
    /// Below the home directory: the path after it.
    Home(PathBuf),
    Absolute(PathBuf),
}

impl Shown {
    /// The skills directory as the warning names the copy's place, with a
    /// character that would break the warning's line shown as U+FFFD.
    fn skills_dir(&self) -> String {
        let (prefix, path) = match self {
            Shown::Relative(path) => ("", path),
            Shown::Home(path) => ("~/", path),
            Shown::Absolute(path) => ("", path),
        };
        let text = format!("{prefix}{}", path.to_string_lossy());
        text.chars()
            .map(|c| if c.is_control() { '\u{fffd}' } else { c })
            .collect()
    }

    /// The copy's directory as the warning names it.
    fn name(&self) -> String {
        format!("{}/antiq", self.skills_dir())
    }

    /// The skills directory for `antiq skill --install`, written so a shell
    /// reads it back as the same directory. A path that cannot be written
    /// that way shows as `<dir>`.
    fn install_dir(&self) -> String {
        const UNWRITABLE: &str = "<dir>";
        match self {
            Shown::Relative(path) | Shown::Absolute(path) => shell_word(path, UNWRITABLE),
            // The shell expands `~/` only unquoted, so the prefix stands
            // outside the quotes the rest may need.
            Shown::Home(path) => match shell_word(path, UNWRITABLE) {
                word if word == UNWRITABLE => word,
                word => format!("~/{word}"),
            },
        }
    }
}

/// A copy older than this antiq.
struct OldCopy {
    shown: Shown,
    from: Stamp,
}

/// The two texts of the warning: what is old, and what to do about it.
#[derive(Debug, PartialEq, Eq)]
struct Outdated {
    message: String,
    fix: String,
}

/// The texts of the warning for `old`, which is not empty. This is the one
/// place the wording lives.
fn hint_text(old: &[OldCopy], running: &str) -> Outdated {
    let from = |stamp: &Stamp| match stamp {
        Stamp::Version(version) => format!("antiq {version}"),
        Stamp::Unstamped => "an older antiq".to_owned(),
    };
    let install = |copy: &OldCopy| format!("`antiq skill --install {}`", copy.shown.install_dir());
    match old {
        [copy] => Outdated {
            message: format!(
                "the antiq skill in {} is from {}, this antiq is {running}",
                copy.shown.name(),
                from(&copy.from),
            ),
            fix: format!(
                "Read the current one with `antiq skill`, and ask the user whether to update the copy with {}.",
                install(copy)
            ),
        },
        copies => {
            let named: Vec<String> = copies
                .iter()
                .map(|copy| format!("{} (from {})", copy.shown.name(), from(&copy.from)))
                .collect();
            let installs: Vec<String> = copies.iter().map(install).collect();
            Outdated {
                message: format!(
                    "the antiq skill copies in {} are older than this antiq, {running}",
                    list(&named)
                ),
                fix: format!(
                    "Read the current skill with `antiq skill`, and ask the user whether to update them with {}.",
                    list(&installs)
                ),
            }
        }
    }
}

/// `path` as one word a shell reads back as the same path: as it is when
/// it holds only ASCII letters, digits and `_./-`, otherwise in single
/// quotes. A path that cannot be written that way, not UTF-8 or holding a
/// control character such as a newline, shows as `placeholder`.
fn shell_word(path: &Path, placeholder: &str) -> String {
    let Some(text) = path
        .to_str()
        .filter(|text| !text.contains(char::is_control))
    else {
        return placeholder.to_owned();
    };
    // A relative path that starts with `-` would read as a flag.
    let text = if path.is_relative() && text.starts_with('-') {
        format!("./{text}")
    } else {
        text.to_owned()
    };
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-');
    if !text.is_empty() && text.chars().all(plain) {
        text
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// `items` as prose: "a", "a and b", "a, b and c".
fn list(items: &[String]) -> String {
    match items
        .split_last()
        .expect("a warning names at least one copy")
    {
        (only, []) => only.clone(),
        (last, others) => format!("{} and {last}", others.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = "0.15.0";

    // =====================================================================
    // Helpers
    // =====================================================================

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temp dir")
    }

    /// The text of a `SKILL.md` that antiq installed, with `stamp` as its
    /// `antiq-version`, or without one as a copy written by hand has
    /// it.
    fn skill_md(stamp: Option<&str>) -> String {
        match stamp {
            Some(version) => format!(
                "---\nname: antiq\ndescription: Ask.\nmetadata:\n  antiq-version: \"{version}\"\n---\n\nBody\n"
            ),
            None => "---\nname: antiq\ndescription: Ask.\n---\n\nBody\n".to_owned(),
        }
    }

    /// Installs a copy under `base/entry/antiq` and returns its directory.
    fn put(base: &Path, entry: &str, stamp: Option<&str>) -> PathBuf {
        let dir = base.join(entry).join("antiq");
        std::fs::create_dir_all(&dir).expect("a creatable directory");
        std::fs::write(dir.join("SKILL.md"), skill_md(stamp)).expect("a writable file");
        dir
    }

    fn run(cwd: &Path, home: Option<&Path>, stop: Option<&Path>) -> Option<String> {
        run_with_config(cwd, home, None, stop)
    }

    fn run_with_config(
        cwd: &Path,
        home: Option<&Path>,
        claude_config: Option<&Path>,
        stop: Option<&Path>,
    ) -> Option<String> {
        let variables: Vec<(&str, &Path)> = claude_config
            .map(|dir| ("CLAUDE_CONFIG_DIR", dir))
            .into_iter()
            .collect();
        run_with_variables(GLOBAL_DIRS, cwd, home, &variables, stop)
    }

    /// A search over the global directories `global`, where the
    /// environment holds exactly `variables`.
    fn run_with_variables(
        global: &[GlobalDir],
        cwd: &Path,
        home: Option<&Path>,
        variables: &[(&str, &Path)],
        stop: Option<&Path>,
    ) -> Option<String> {
        check(&Search {
            cwd,
            home,
            global,
            variable: &|name| {
                variables
                    .iter()
                    .find(|(known, _)| *known == name)
                    .map(|(_, value)| OsString::from(value))
            },
            stop,
            running: RUNNING,
        })
        .map(joined)
    }

    /// The warning's message and fix as one line, the way the tests below
    /// compare them.
    fn joined(outdated: Outdated) -> String {
        format!("{}; {}", outdated.message, outdated.fix)
    }

    /// The text of the hint for one copy installed by `0.14.0` under
    /// `.claude/skills` of the current directory.
    const ONE_OLD_COPY: &str = "the antiq skill in .claude/skills/antiq is from antiq 0.14.0, this antiq is 0.15.0; Read the current one with `antiq skill`, and ask the user whether to update the copy with `antiq skill --install .claude/skills`.";

    // =====================================================================
    // What is reported
    // =====================================================================

    #[test]
    fn a_project_copy_from_an_older_antiq_is_named_with_its_version() {
        let project = temp();
        put(project.path(), ".claude/skills", Some("0.14.0"));

        let hint = run(project.path(), None, Some(project.path()));

        assert_eq!(hint.as_deref(), Some(ONE_OLD_COPY));
    }

    #[test]
    fn a_copy_without_a_stamp_is_from_an_older_antiq() {
        let project = temp();
        put(project.path(), ".claude/skills", None);

        let hint = run(project.path(), None, Some(project.path()));

        assert_eq!(
            hint.as_deref(),
            Some(
                "the antiq skill in .claude/skills/antiq is from an older antiq, this antiq is 0.15.0; Read the current one with `antiq skill`, and ask the user whether to update the copy with `antiq skill --install .claude/skills`."
            )
        );
    }

    #[test]
    fn a_global_copy_is_shown_under_the_home_directory() {
        let home = temp();
        let project = temp();
        put(home.path(), ".copilot/skills", Some("0.14.0"));

        let hint = run(project.path(), Some(home.path()), Some(project.path()));

        assert_eq!(
            hint.as_deref(),
            Some(
                "the antiq skill in ~/.copilot/skills/antiq is from antiq 0.14.0, this antiq is 0.15.0; Read the current one with `antiq skill`, and ask the user whether to update the copy with `antiq skill --install ~/.copilot/skills`."
            )
        );
    }

    #[test]
    fn several_old_copies_go_into_one_hint() {
        let home = temp();
        let project = temp();
        put(project.path(), ".claude/skills", Some("0.14.0"));
        put(home.path(), ".agents/skills", None);

        let hint = run(project.path(), Some(home.path()), Some(project.path()));

        assert_eq!(
            hint.as_deref(),
            Some(
                "the antiq skill copies in .claude/skills/antiq (from antiq 0.14.0) and ~/.agents/skills/antiq (from an older antiq) are older than this antiq, 0.15.0; Read the current skill with `antiq skill`, and ask the user whether to update them with `antiq skill --install .claude/skills` and `antiq skill --install ~/.agents/skills`."
            )
        );
    }

    #[test]
    fn three_old_copies_are_listed_with_commas_and_an_and() {
        let old = |dir: &str, from| OldCopy {
            shown: Shown::Relative(dir.into()),
            from,
        };
        let copies = [
            old(".agents/skills", Stamp::Unstamped),
            old(".claude/skills", Stamp::Version(Version(0, 14, 0))),
            old(".codex/skills", Stamp::Version(Version(0, 9, 2))),
        ];

        assert_eq!(
            joined(hint_text(&copies, RUNNING)),
            "the antiq skill copies in .agents/skills/antiq (from an older antiq), .claude/skills/antiq (from antiq 0.14.0) and .codex/skills/antiq (from antiq 0.9.2) are older than this antiq, 0.15.0; Read the current skill with `antiq skill`, and ask the user whether to update them with `antiq skill --install .agents/skills`, `antiq skill --install .claude/skills` and `antiq skill --install .codex/skills`."
        );
    }

    #[test]
    fn every_directory_of_both_tables_is_searched() {
        for entry in PROJECT_DIRS {
            let project = temp();
            put(project.path(), entry, Some("0.14.0"));

            let hint = run(project.path(), None, Some(project.path()));

            let named = format!("the antiq skill in {entry}/antiq is from antiq 0.14.0");
            assert!(
                hint.as_deref().is_some_and(|hint| hint.starts_with(&named)),
                "project {entry}: {hint:?}"
            );
        }
        for dir in GLOBAL_DIRS {
            let entry = dir.home_relative;
            let home = temp();
            let project = temp();
            put(home.path(), entry, Some("0.14.0"));

            let hint = run(project.path(), Some(home.path()), Some(project.path()));

            let named = format!("the antiq skill in ~/{entry}/antiq is from antiq 0.14.0");
            assert!(
                hint.as_deref().is_some_and(|hint| hint.starts_with(&named)),
                "global {entry}: {hint:?}"
            );
        }
    }

    #[test]
    fn a_codex_directory_copy_in_the_home_directory_is_named_under_it() {
        let home = temp();
        let project = temp();
        put(home.path(), ".codex/skills", Some("0.14.0"));

        let hint = run(project.path(), Some(home.path()), Some(project.path()));

        assert_eq!(
            hint.as_deref(),
            Some(
                "the antiq skill in ~/.codex/skills/antiq is from antiq 0.14.0, this antiq is 0.15.0; Read the current one with `antiq skill`, and ask the user whether to update the copy with `antiq skill --install ~/.codex/skills`."
            )
        );
    }

    #[test]
    fn without_a_home_only_project_copies_are_searched() {
        let project = temp();
        put(project.path(), ".claude/skills", Some("0.14.0"));

        let hint = run(project.path(), None, Some(project.path()));

        assert_eq!(hint.as_deref(), Some(ONE_OLD_COPY));
    }

    // =====================================================================
    // The versions that count as older
    // =====================================================================

    #[test]
    fn a_copy_of_the_running_version_gives_no_hint() {
        let project = temp();
        put(project.path(), ".claude/skills", Some(RUNNING));

        assert_eq!(run(project.path(), None, Some(project.path())), None);
    }

    #[test]
    fn a_newer_copy_gives_no_hint() {
        let project = temp();
        put(project.path(), ".claude/skills", Some("0.16.0"));

        assert_eq!(run(project.path(), None, Some(project.path())), None);
    }

    #[test]
    fn a_copy_with_a_stamp_that_is_no_version_gives_no_hint() {
        let project = temp();
        put(project.path(), ".claude/skills", Some("banana"));

        assert_eq!(run(project.path(), None, Some(project.path())), None);
    }

    #[test]
    fn a_current_copy_does_not_hide_an_older_one() {
        let project = temp();
        put(project.path(), ".agents/skills", Some(RUNNING));
        put(project.path(), ".claude/skills", Some("0.14.0"));

        let hint = run(project.path(), None, Some(project.path()));

        assert_eq!(hint.as_deref(), Some(ONE_OLD_COPY));
    }

    #[test]
    fn versions_compare_by_number_and_not_by_text() {
        assert!(Version(0, 9, 2) < Version(0, 14, 0));
        assert_eq!(parse_version("0.9.2"), Some(Version(0, 9, 2)));
        assert_eq!(parse_version("0.14.0"), Some(Version(0, 14, 0)));
    }

    #[test]
    fn a_pre_release_or_build_suffix_does_not_count() {
        assert_eq!(parse_version("0.15.0-rc.1"), Some(Version(0, 15, 0)));
        assert_eq!(parse_version("0.15.0+abc"), Some(Version(0, 15, 0)));
    }

    #[test]
    fn a_text_that_is_no_three_part_version_is_refused() {
        for text in [
            "", "banana", "0.14", "0.14.0.1", "0.-1.0", "0.+1.0", "0..0", "v0.14.0",
        ] {
            assert_eq!(parse_version(text), None, "{text:?}");
        }
    }

    // =====================================================================
    // The stamp in a SKILL.md
    // =====================================================================

    fn version_of(text: &str) -> Option<Stamp> {
        stamp_of(text)
    }

    const V014: Option<Stamp> = Some(Stamp::Version(Version(0, 14, 0)));

    #[test]
    fn the_stamp_is_read_under_metadata() {
        assert_eq!(version_of(&skill_md(Some("0.14.0"))), V014);
    }

    #[test]
    fn a_stamp_in_single_quotes_counts() {
        assert_eq!(version_of("---\nantiq-version: '0.14.0'\n---\n"), V014);
    }

    #[test]
    fn a_stamp_without_quotes_counts() {
        assert_eq!(version_of("---\nantiq-version: 0.14.0\n---\n"), V014);
    }

    #[test]
    fn a_stamp_at_the_top_level_counts() {
        assert_eq!(
            version_of("---\nname: antiq\nantiq-version: \"0.14.0\"\n---\n"),
            V014
        );
    }

    #[test]
    fn a_stamp_in_a_file_with_windows_line_endings_counts() {
        assert_eq!(
            version_of("---\r\nmetadata:\r\n  antiq-version: \"0.14.0\"\r\n---\r\n\r\nBody\r\n"),
            V014
        );
    }

    #[test]
    fn a_frontmatter_without_a_stamp_is_unstamped() {
        assert_eq!(version_of(&skill_md(None)), Some(Stamp::Unstamped));
    }

    #[test]
    fn a_text_without_frontmatter_is_unstamped() {
        assert_eq!(version_of("# antiq\n"), Some(Stamp::Unstamped));
    }

    #[test]
    fn a_frontmatter_that_never_ends_is_unstamped() {
        assert_eq!(
            version_of("---\nantiq-version: \"0.14.0\"\n"),
            Some(Stamp::Unstamped)
        );
    }

    #[test]
    fn a_line_in_the_body_is_no_stamp() {
        assert_eq!(
            version_of("---\nname: antiq\n---\n\nantiq-version: \"0.14.0\"\n"),
            Some(Stamp::Unstamped)
        );
    }

    #[test]
    fn another_key_that_starts_alike_is_no_stamp() {
        assert_eq!(
            version_of("---\nantiq-version-of-the-docs: \"0.14.0\"\n---\n"),
            Some(Stamp::Unstamped)
        );
    }

    #[test]
    fn a_stamp_that_is_no_version_is_refused() {
        assert_eq!(version_of("---\nantiq-version: \"banana\"\n---\n"), None);
        assert_eq!(version_of("---\nantiq-version: \"\"\n---\n"), None);
    }

    #[test]
    fn the_skill_this_antiq_ships_carries_its_own_version() {
        let running = parse_version(env!("CARGO_PKG_VERSION")).expect("a crate version");

        assert_eq!(
            stamp_of(super::super::cmd::skill::SKILL),
            Some(Stamp::Version(running))
        );
    }

    // =====================================================================
    // The directory CLAUDE_CONFIG_DIR names
    // =====================================================================

    fn config_dir(value: &str) -> Option<PathBuf> {
        override_dir(Some(OsString::from(value)))
    }

    #[test]
    fn an_unset_claude_config_dir_names_no_directory() {
        assert_eq!(override_dir(None), None);
    }

    #[test]
    fn an_empty_claude_config_dir_names_no_directory() {
        assert_eq!(config_dir(""), None);
    }

    #[test]
    fn a_relative_claude_config_dir_names_no_directory() {
        assert_eq!(config_dir("rel"), None);
    }

    #[test]
    fn a_claude_config_dir_with_a_literal_tilde_names_no_directory() {
        // Nothing expands the `~` in the environment, so the path is
        // relative.
        assert_eq!(config_dir("~/.claude-work"), None);
    }

    #[test]
    fn an_absolute_claude_config_dir_is_the_directory() {
        assert_eq!(
            config_dir("/home/me/.claude-work"),
            Some(PathBuf::from("/home/me/.claude-work"))
        );
    }

    #[test]
    fn a_copy_in_the_claude_config_dir_below_home_is_named_under_home() {
        let home = temp();
        let project = temp();
        let config = home.path().join(".claude-work");
        put(&config, "skills", Some("0.14.0"));

        let hint = run_with_config(
            project.path(),
            Some(home.path()),
            Some(&config),
            Some(project.path()),
        );

        assert_eq!(
            hint.as_deref(),
            Some(
                "the antiq skill in ~/.claude-work/skills/antiq is from antiq 0.14.0, this antiq is 0.15.0; Read the current one with `antiq skill`, and ask the user whether to update the copy with `antiq skill --install ~/.claude-work/skills`."
            )
        );
    }

    #[test]
    fn a_copy_in_the_claude_config_dir_outside_home_is_named_absolute() {
        let home = temp();
        let project = temp();
        let config = temp();
        put(config.path(), "skills", Some("0.14.0"));

        let hint = run_with_config(
            project.path(),
            Some(home.path()),
            Some(config.path()),
            Some(project.path()),
        )
        .expect("a hint");

        let skills = config.path().join("skills");
        assert!(
            hint.starts_with(&format!(
                "the antiq skill in {}/antiq is from antiq 0.14.0",
                skills.display()
            )),
            "{hint}"
        );
        assert!(
            hint.ends_with(&format!("`antiq skill --install {}`.", skills.display())),
            "{hint}"
        );
    }

    #[test]
    fn the_claude_config_dir_is_searched_without_a_home() {
        let project = temp();
        let config = temp();
        put(config.path(), "skills", Some("0.14.0"));

        let hint = run_with_config(project.path(), None, Some(config.path()), None);

        assert!(hint.is_some_and(|hint| hint.contains("is from antiq 0.14.0")));
    }

    #[test]
    fn the_claude_config_dir_that_is_the_default_directory_is_named_once() {
        let home = temp();
        let project = temp();
        let config = home.path().join(".claude");
        put(home.path(), ".claude/skills", Some("0.14.0"));

        let hint = run_with_config(
            project.path(),
            Some(home.path()),
            Some(&config),
            Some(project.path()),
        )
        .expect("a hint");

        assert!(
            hint.starts_with("the antiq skill in ~/.claude/skills/antiq is from"),
            "{hint}"
        );
        assert!(!hint.contains("copies"), "{hint}");
    }

    #[cfg(unix)]
    #[test]
    fn a_claude_config_dir_whose_skills_link_to_the_default_is_named_once() {
        let home = temp();
        let project = temp();
        put(home.path(), ".claude/skills", Some("0.14.0"));
        let config = home.path().join(".claude-x");
        std::fs::create_dir_all(&config).expect("a creatable directory");
        std::os::unix::fs::symlink("../.claude/skills", config.join("skills")).expect("a symlink");

        let hint = run_with_config(
            project.path(),
            Some(home.path()),
            Some(&config),
            Some(project.path()),
        )
        .expect("a hint");

        assert!(
            hint.starts_with("the antiq skill in ~/.claude/skills/antiq is from"),
            "{hint}"
        );
        assert!(!hint.contains("claude-x"), "{hint}");
    }

    #[test]
    fn the_claude_config_dir_comes_right_after_the_default_claude_directory() {
        let home = temp();
        let project = temp();
        let config = home.path().join(".cfg");
        put(home.path(), ".gemini/skills", Some("0.12.0"));
        put(&config, "skills", Some("0.13.0"));
        put(home.path(), ".claude/skills", Some("0.14.0"));

        let hint = run_with_config(
            project.path(),
            Some(home.path()),
            Some(&config),
            Some(project.path()),
        )
        .expect("a hint");

        let at = |name: &str| {
            hint.find(name)
                .unwrap_or_else(|| panic!("{name} in {hint}"))
        };
        assert!(
            at("~/.claude/skills/antiq") < at("~/.cfg/skills/antiq")
                && at("~/.cfg/skills/antiq") < at("~/.gemini/skills/antiq"),
            "{hint}"
        );
    }

    /// A table of two agents, only the second of which can be moved, by
    /// a variable that no real agent uses.
    const MADE_UP: &[GlobalDir] = &[
        GlobalDir {
            home_relative: ".first/skills",
            moved_by: None,
        },
        GlobalDir {
            home_relative: ".second/skills",
            moved_by: Some(Override {
                variable: "SECOND_HOME",
                subpath: "agent/skills",
            }),
        },
    ];

    #[test]
    fn any_table_entry_can_name_a_variable_and_a_subpath() {
        let home = temp();
        let project = temp();
        let moved = temp();
        put(moved.path(), "agent/skills", Some("0.14.0"));

        let hint = run_with_variables(
            MADE_UP,
            project.path(),
            Some(home.path()),
            &[("SECOND_HOME", moved.path())],
            Some(project.path()),
        )
        .expect("a hint");

        let skills = moved.path().join("agent/skills");
        assert!(
            hint.starts_with(&format!(
                "the antiq skill in {}/antiq is from",
                skills.display()
            )),
            "{hint}"
        );
    }

    #[test]
    fn a_variable_of_another_entry_moves_nothing() {
        let home = temp();
        let project = temp();
        let moved = temp();
        put(moved.path(), "agent/skills", Some("0.14.0"));

        let hint = run_with_variables(
            MADE_UP,
            project.path(),
            Some(home.path()),
            &[("FIRST_HOME", moved.path())],
            Some(project.path()),
        );

        assert_eq!(hint, None);
    }

    #[test]
    fn the_default_directory_of_a_moved_entry_stays_searched() {
        let home = temp();
        let project = temp();
        let moved = temp();
        put(home.path(), ".second/skills", Some("0.13.0"));
        put(moved.path(), "agent/skills", Some("0.14.0"));

        let hint = run_with_variables(
            MADE_UP,
            project.path(),
            Some(home.path()),
            &[("SECOND_HOME", moved.path())],
            Some(project.path()),
        )
        .expect("a hint");

        assert!(
            hint.contains("~/.second/skills/antiq (from antiq 0.13.0)"),
            "{hint}"
        );
        assert!(
            hint.contains("agent/skills/antiq (from antiq 0.14.0)"),
            "{hint}"
        );
    }

    #[test]
    fn the_override_variables_are_those_of_the_table_each_once() {
        let names: Vec<&str> = override_variables().collect();

        assert_eq!(
            names,
            [
                "GEMINI_CLI_HOME",
                "CLAUDE_CONFIG_DIR",
                "COPILOT_HOME",
                "CODEX_HOME"
            ]
        );
    }

    /// The variable `variable` holding a directory in which the copy sits
    /// at `subpath/antiq` is honoured: the copy is named by that path.
    fn honours(variable: &str, subpath: &str) {
        let home = temp();
        let project = temp();
        let moved = temp();
        put(moved.path(), subpath, Some("0.14.0"));

        let hint = run_with_variables(
            GLOBAL_DIRS,
            project.path(),
            Some(home.path()),
            &[(variable, moved.path())],
            Some(project.path()),
        );

        let skills = moved.path().join(subpath);
        let named = format!(
            "the antiq skill in {}/antiq is from antiq 0.14.0",
            skills.display()
        );
        assert!(
            hint.as_deref().is_some_and(|hint| hint.starts_with(&named)),
            "{variable}: {hint:?}"
        );
    }

    #[test]
    fn claude_config_dir_moves_the_claude_directory() {
        honours("CLAUDE_CONFIG_DIR", "skills");
    }

    #[test]
    fn codex_home_moves_the_codex_directory() {
        honours("CODEX_HOME", "skills");
    }

    #[test]
    fn copilot_home_moves_the_copilot_directory() {
        honours("COPILOT_HOME", "skills");
    }

    #[test]
    fn gemini_cli_home_moves_the_gemini_directory() {
        honours("GEMINI_CLI_HOME", ".gemini/skills");
    }

    #[test]
    fn gemini_cli_home_moves_the_shared_directory() {
        honours("GEMINI_CLI_HOME", ".agents/skills");
    }

    #[test]
    fn the_cursor_directory_has_no_variable() {
        let moved = temp();
        let home = temp();
        let project = temp();
        put(moved.path(), "skills", Some("0.14.0"));
        let variables: Vec<(&str, &Path)> = ["CURSOR_CONFIG_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|name| (name, moved.path()))
            .collect();

        let hint = run_with_variables(
            GLOBAL_DIRS,
            project.path(),
            Some(home.path()),
            &variables,
            Some(project.path()),
        );

        assert_eq!(hint, None);
    }

    #[test]
    fn a_claude_config_dir_with_a_space_is_quoted_after_the_tilde() {
        let home = temp();
        let project = temp();
        let config = home.path().join("my cfg");
        put(&config, "skills", Some("0.14.0"));

        let hint = run_with_config(
            project.path(),
            Some(home.path()),
            Some(&config),
            Some(project.path()),
        )
        .expect("a hint");

        assert!(
            hint.ends_with("`antiq skill --install ~/'my cfg/skills'`."),
            "{hint}"
        );
    }

    // =====================================================================
    // How a copy is shown
    // =====================================================================

    fn one_copy(shown: Shown) -> String {
        joined(hint_text(
            &[OldCopy {
                shown,
                from: Stamp::Unstamped,
            }],
            RUNNING,
        ))
    }

    #[test]
    fn a_path_with_a_space_is_quoted_in_the_install_command() {
        let hint = one_copy(Shown::Absolute("/work/my project/.claude/skills".into()));

        assert!(hint.starts_with("the antiq skill in /work/my project/.claude/skills/antiq is"));
        assert!(hint.ends_with("`antiq skill --install '/work/my project/.claude/skills'`."));
    }

    #[test]
    fn the_tilde_stays_outside_the_quotes_of_a_home_path() {
        let hint = one_copy(Shown::Home("my project/.claude/skills".into()));

        assert!(hint.ends_with("`antiq skill --install ~/'my project/.claude/skills'`."));
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_no_utf_8_shows_as_the_placeholder_in_the_install_command() {
        use std::os::unix::ffi::OsStrExt;

        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(
            b"/work/gr\xf6\xdfe/.claude/skills",
        ));
        for shown in [Shown::Absolute(odd.clone()), Shown::Home(odd)] {
            assert!(one_copy(shown).ends_with("`antiq skill --install <dir>`."));
        }
    }

    #[test]
    fn a_control_character_in_a_path_cannot_break_the_line() {
        let hint = one_copy(Shown::Absolute("/work/a\nb/.claude/skills".into()));

        assert!(!hint.contains('\n'), "{hint:?}");
        assert!(
            hint.contains("/work/a\u{fffd}b/.claude/skills/antiq is"),
            "{hint:?}"
        );
        assert!(hint.ends_with("`antiq skill --install <dir>`."), "{hint:?}");
    }

    #[test]
    fn a_relative_directory_that_starts_with_a_dash_is_not_read_as_a_flag() {
        let hint = one_copy(Shown::Relative("-odd/skills".into()));

        assert!(
            hint.ends_with("`antiq skill --install ./-odd/skills`."),
            "{hint}"
        );
    }

    #[test]
    fn a_project_copy_above_the_current_directory_is_shown_absolute() {
        let outer = temp();
        let project = outer.path().join("project");
        let sub = project.join("sub");
        std::fs::create_dir_all(&sub).expect("a creatable directory");
        let home = temp();
        put(&project, ".claude/skills", Some("0.14.0"));

        let hint = run(&sub, Some(home.path()), Some(&project)).expect("a hint");

        let at = project.join(".claude/skills");
        assert!(
            hint.starts_with(&format!(
                "the antiq skill in {}/antiq is from antiq 0.14.0",
                at.display()
            )),
            "{hint}"
        );
        assert!(
            hint.ends_with(&format!("`antiq skill --install {}`.", at.display())),
            "{hint}"
        );
    }

    #[test]
    fn a_project_copy_above_the_current_directory_under_home_is_shown_with_a_tilde() {
        let home = temp();
        let project = home.path().join("project");
        let sub = project.join("sub");
        std::fs::create_dir_all(&sub).expect("a creatable directory");
        put(&project, ".claude/skills", Some("0.14.0"));

        let hint = run(&sub, Some(home.path()), Some(&project)).expect("a hint");

        assert!(
            hint.starts_with(
                "the antiq skill in ~/project/.claude/skills/antiq is from antiq 0.14.0"
            ),
            "{hint}"
        );
        assert!(
            hint.ends_with("`antiq skill --install ~/project/.claude/skills`."),
            "{hint}"
        );
    }

    // =====================================================================
    // Where the walk goes
    // =====================================================================

    #[test]
    fn a_copy_above_the_git_root_is_not_found() {
        let outer = temp();
        let project = outer.path().join("project");
        let sub = project.join("sub");
        std::fs::create_dir_all(&sub).expect("a creatable directory");
        put(outer.path(), ".claude/skills", Some("0.14.0"));

        assert_eq!(run(&sub, None, Some(&project)), None);
    }

    #[test]
    fn without_a_git_root_only_the_current_directory_is_searched() {
        let project = temp();
        let sub = project.path().join("sub");
        std::fs::create_dir_all(&sub).expect("a creatable directory");
        put(project.path(), ".claude/skills", Some("0.14.0"));

        assert_eq!(run(&sub, None, None), None);
    }

    #[test]
    fn a_stop_that_is_no_ancestor_of_the_current_directory_searches_it_alone() {
        let project = temp();
        let sub = project.path().join("sub");
        let elsewhere = temp();
        std::fs::create_dir_all(&sub).expect("a creatable directory");
        put(project.path(), ".claude/skills", Some("0.14.0"));

        assert_eq!(run(&sub, None, Some(elsewhere.path())), None);
    }

    #[test]
    fn the_current_directory_is_searched_before_those_above() {
        let project = temp();
        let sub = project.path().join("sub");
        std::fs::create_dir_all(&sub).expect("a creatable directory");
        put(project.path(), ".agents/skills", Some("0.13.0"));
        put(&sub, ".claude/skills", Some("0.14.0"));

        let hint = run(&sub, None, Some(project.path())).expect("a hint");

        let here = hint.find(".claude/skills/antiq (from antiq 0.14.0)");
        let above = hint.find("/.agents/skills/antiq (from antiq 0.13.0)");
        assert!(here.is_some() && above.is_some() && here < above, "{hint}");
    }

    #[test]
    fn a_git_directory_marks_the_root() {
        let root = temp();
        let sub = root.path().join("a/b");
        std::fs::create_dir_all(&sub).expect("a creatable directory");
        std::fs::create_dir(root.path().join(".git")).expect("a creatable directory");

        assert_eq!(
            git_root(&sub, Some(root.path())),
            Some(root.path().to_owned())
        );
    }

    #[test]
    fn a_git_file_of_a_linked_worktree_marks_the_root() {
        let root = temp();
        let sub = root.path().join("a/b");
        std::fs::create_dir_all(&sub).expect("a creatable directory");
        std::fs::write(
            root.path().join(".git"),
            "gitdir: /elsewhere/.git/worktrees/x\n",
        )
        .expect("a writable file");

        assert_eq!(
            git_root(&sub, Some(root.path())),
            Some(root.path().to_owned())
        );
    }

    #[test]
    fn the_nearest_git_root_wins() {
        let outer = temp();
        let inner = outer.path().join("inner");
        let sub = inner.join("sub");
        std::fs::create_dir_all(&sub).expect("a creatable directory");
        std::fs::create_dir(outer.path().join(".git")).expect("a creatable directory");
        std::fs::create_dir(inner.join(".git")).expect("a creatable directory");

        assert_eq!(git_root(&sub, Some(outer.path())), Some(inner));
    }

    #[test]
    fn the_git_search_does_not_go_above_its_ceiling() {
        let outer = temp();
        let inner = outer.path().join("inner");
        std::fs::create_dir_all(&inner).expect("a creatable directory");
        std::fs::create_dir(outer.path().join(".git")).expect("a creatable directory");

        assert_eq!(git_root(&inner, Some(&inner)), None);
    }

    #[test]
    fn a_git_file_stops_the_walk_below_a_repository() {
        let outer = temp();
        let root = outer.path().join("worktree");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).expect("a creatable directory");
        std::fs::create_dir(outer.path().join(".git")).expect("a creatable directory");
        std::fs::write(root.join(".git"), "gitdir: /elsewhere\n").expect("a writable file");
        put(&root, ".claude/skills", Some("0.14.0"));
        put(outer.path(), ".agents/skills", Some("0.13.0"));

        let stop = git_root(&sub, Some(outer.path()));
        let hint = run(&sub, None, stop.as_deref()).expect("a hint");

        assert!(
            hint.contains("/.claude/skills/antiq is from antiq 0.14.0"),
            "{hint}"
        );
        assert!(!hint.contains(".agents"), "{hint}");
    }

    // =====================================================================
    // One copy, however many ways lead to it
    // =====================================================================

    #[cfg(unix)]
    #[test]
    fn a_copy_reached_through_a_symlink_is_named_once() {
        let project = temp();
        let real = put(project.path(), ".agents/skills", Some("0.14.0"));
        std::fs::create_dir_all(project.path().join(".claude/skills"))
            .expect("a creatable directory");
        std::os::unix::fs::symlink(&real, project.path().join(".claude/skills/antiq"))
            .expect("a symlink");

        let hint = run(project.path(), None, Some(project.path()));

        let hint = hint.expect("a hint");
        assert!(
            hint.starts_with("the antiq skill in .agents/skills/antiq is from"),
            "{hint}"
        );
        assert!(!hint.contains(".claude"), "{hint}");
    }

    #[cfg(unix)]
    #[test]
    fn a_skills_directory_that_is_a_symlink_is_named_once() {
        let project = temp();
        put(project.path(), ".agents/skills", Some("0.14.0"));
        std::fs::create_dir_all(project.path().join(".claude")).expect("a creatable directory");
        std::os::unix::fs::symlink(
            project.path().join(".agents/skills"),
            project.path().join(".claude/skills"),
        )
        .expect("a symlink");

        let hint = run(project.path(), None, Some(project.path())).expect("a hint");

        assert!(
            hint.starts_with("the antiq skill in .agents/skills/antiq is from"),
            "{hint}"
        );
        assert!(!hint.contains(".claude"), "{hint}");
    }

    #[cfg(unix)]
    #[test]
    fn a_skill_file_shared_by_two_copies_is_named_once() {
        let project = temp();
        let real = put(project.path(), ".agents/skills", Some("0.14.0"));
        let other = project.path().join(".claude/skills/antiq");
        std::fs::create_dir_all(&other).expect("a creatable directory");
        std::os::unix::fs::symlink(real.join("SKILL.md"), other.join("SKILL.md"))
            .expect("a symlink");

        let hint = run(project.path(), None, Some(project.path())).expect("a hint");

        assert!(
            hint.starts_with("the antiq skill in .agents/skills/antiq is from"),
            "{hint}"
        );
        assert!(!hint.contains(".claude"), "{hint}");
    }

    #[test]
    fn two_separate_copies_of_one_version_are_both_named() {
        let project = temp();
        put(project.path(), ".agents/skills", Some("0.14.0"));
        put(project.path(), ".claude/skills", Some("0.14.0"));

        let hint = run(project.path(), None, Some(project.path())).expect("a hint");

        assert!(hint.contains(".agents/skills/antiq (from"), "{hint}");
        assert!(hint.contains(".claude/skills/antiq (from"), "{hint}");
    }

    #[test]
    fn a_project_that_is_the_home_directory_names_a_copy_once() {
        let home = temp();
        put(home.path(), ".claude/skills", Some("0.14.0"));

        let hint = run(home.path(), Some(home.path()), Some(home.path())).expect("a hint");

        assert!(
            hint.starts_with("the antiq skill in .claude/skills/antiq is from"),
            "{hint}"
        );
        assert!(!hint.contains("copies"), "{hint}");
    }

    // =====================================================================
    // What cannot be read gives no hint
    // =====================================================================

    #[test]
    fn a_directory_without_a_skill_file_gives_no_hint() {
        let project = temp();
        std::fs::create_dir_all(project.path().join(".claude/skills/antiq"))
            .expect("a creatable directory");

        assert_eq!(run(project.path(), None, Some(project.path())), None);
    }

    #[test]
    fn a_skill_file_that_is_a_directory_gives_no_hint() {
        let project = temp();
        std::fs::create_dir_all(project.path().join(".claude/skills/antiq/SKILL.md"))
            .expect("a creatable directory");

        assert_eq!(run(project.path(), None, Some(project.path())), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_leads_nowhere_gives_no_hint() {
        let project = temp();
        std::fs::create_dir_all(project.path().join(".claude/skills"))
            .expect("a creatable directory");
        std::os::unix::fs::symlink(
            project.path().join("missing"),
            project.path().join(".claude/skills/antiq"),
        )
        .expect("a symlink");

        assert_eq!(run(project.path(), None, Some(project.path())), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_skill_file_that_links_to_itself_gives_no_hint() {
        let project = temp();
        let dir = project.path().join(".claude/skills/antiq");
        std::fs::create_dir_all(&dir).expect("a creatable directory");
        std::os::unix::fs::symlink(dir.join("SKILL.md"), dir.join("SKILL.md")).expect("a symlink");

        assert_eq!(run(project.path(), None, Some(project.path())), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_skill_file_without_read_permission_gives_no_hint() {
        use std::os::unix::fs::PermissionsExt;

        let project = temp();
        let dir = put(project.path(), ".claude/skills", Some("0.14.0"));
        let file = dir.join("SKILL.md");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000))
            .expect("changeable permissions");
        // A superuser reads it anyway, and then the copy is old.
        let readable = std::fs::File::open(&file).is_ok();

        let hint = run(project.path(), None, Some(project.path()));

        assert_eq!(hint.is_some(), readable);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_as_skill_file_does_not_block_the_check() {
        let project = temp();
        let dir = project.path().join(".claude/skills/antiq");
        std::fs::create_dir_all(&dir).expect("a creatable directory");
        let made = std::process::Command::new("mkfifo")
            .arg(dir.join("SKILL.md"))
            .status()
            .expect("mkfifo runs");
        assert!(made.success());

        let path = project.path().to_owned();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(run(&path, None, Some(&path)));
        });
        let hint = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the check must not block on a FIFO");

        assert_eq!(hint, None);
    }

    // =====================================================================
    // How much of a file is read
    // =====================================================================

    #[test]
    fn a_stamp_is_found_when_a_multibyte_character_ends_the_prefix_read() {
        let project = temp();
        let dir = project.path().join(".claude/skills/antiq");
        std::fs::create_dir_all(&dir).expect("a creatable directory");
        // The arrow takes three bytes and starts one byte before the end
        // of the prefix.
        let mut text = skill_md(Some("0.14.0"));
        text.push_str(&"a".repeat(PREFIX - text.len() - 1));
        text.push('\u{2192}');
        text.push_str("tail");
        std::fs::write(dir.join("SKILL.md"), text).expect("a writable file");

        let hint = run(project.path(), None, Some(project.path()));

        assert_eq!(hint.as_deref(), Some(ONE_OLD_COPY));
    }

    #[test]
    fn a_stamp_is_found_when_the_body_is_no_utf_8() {
        let project = temp();
        let dir = project.path().join(".claude/skills/antiq");
        std::fs::create_dir_all(&dir).expect("a creatable directory");
        let mut bytes = skill_md(Some("0.14.0")).into_bytes();
        bytes.extend_from_slice(b"gr\xf6\xdfe");
        std::fs::write(dir.join("SKILL.md"), bytes).expect("a writable file");

        let hint = run(project.path(), None, Some(project.path()));

        assert_eq!(hint.as_deref(), Some(ONE_OLD_COPY));
    }

    #[test]
    fn a_stamp_after_the_prefix_is_not_read() {
        let project = temp();
        let dir = project.path().join(".claude/skills/antiq");
        std::fs::create_dir_all(&dir).expect("a creatable directory");
        let mut text = String::from("---\nname: antiq\n");
        text.push_str(&"# padding\n".repeat(PREFIX / 10 + 1));
        text.push_str("antiq-version: \"0.14.0\"\n---\n");
        std::fs::write(dir.join("SKILL.md"), text).expect("a writable file");

        let hint = run(project.path(), None, Some(project.path())).expect("a hint");

        assert!(hint.contains("is from an older antiq"), "{hint}");
    }
}

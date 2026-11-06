// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The collection's config file.
//!
//! `[collection]` and `[target]` are read leniently: a key this version does
//! not know is ignored, so that a config written by a newer version still
//! reads, and callers warn about each one, since a misspelled key would
//! otherwise pass unnoticed. The keys of the earlier multi-machine design
//! are refused.

use crate::id::ObjectId;
use crate::relpath::RelPath;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{Array, DocumentMut};

/// Errors of the config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read the config file {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config{}:\n{source}", at(path))]
    Parse {
        path: Option<PathBuf>,
        source: toml::de::Error,
    },
    #[error("invalid config{}:\n{source}", at(path))]
    Edit {
        path: Option<PathBuf>,
        source: toml_edit::TomlError,
    },
    /// Keys and tables of the earlier design, all named at once so that one
    /// edit of the file clears them.
    #[error("invalid config{}: these settings no longer exist: {}", at(path), keys.join(", "))]
    RemovedKeys {
        path: Option<PathBuf>,
        keys: Vec<String>,
    },
}

fn at(path: &Option<PathBuf>) -> String {
    match path {
        Some(path) => format!(" in {}", path.display()),
        None => String::new(),
    }
}

impl crate::error::HasKind for ConfigError {}

/// The parsed `config.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Config {
    pub collection: CollectionConfig,
    #[serde(default)]
    pub target: TargetConfig,
    /// Keys of `[collection]` that this version does not know.
    #[serde(skip)]
    pub unknown_collection_keys: Vec<String>,
    /// Keys of `[target]` that this version does not know.
    #[serde(skip)]
    pub unknown_target_keys: Vec<String>,
}

/// The `[collection]` table. Every key but `id` has a default.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CollectionConfig {
    pub id: ObjectId,
    #[serde(default = "defaults::patterns")]
    pub patterns: Vec<String>,
    /// Literal paths that are never pieces, in the escaped form of the
    /// listing format.
    #[serde(default)]
    pub exclude: Vec<RelPath>,
    /// Gitignore lines for ignored paths, anchored at each piece's top.
    #[serde(default = "defaults::ignore")]
    pub ignore: Vec<String>,
    /// Gitignore lines for timeless paths, anchored at each piece's top.
    #[serde(default)]
    pub timeless: Vec<String>,
    #[serde(default = "defaults::idle_days")]
    pub idle_days: u32,
    #[serde(default = "defaults::soon_days")]
    pub soon_days: u32,
}

/// The `[target]` table: where `push` mirrors the collection and how.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct TargetConfig {
    /// An absolute path or `[user@]host:path`. Kept as written; the target
    /// code parses it.
    pub url: Option<String>,
    /// The local rsync; `None` searches `PATH`.
    pub rsync: Option<PathBuf>,
    /// The target's rsync, passed as `--rsync-path`.
    pub rsync_path: Option<String>,
    /// Whether to pass `--fsync`.
    pub fsync: bool,
    /// Extra rsync options.
    pub rsync_args: Vec<String>,
}

impl Default for TargetConfig {
    fn default() -> Self {
        TargetConfig {
            url: None,
            rsync: None,
            rsync_path: None,
            fsync: true,
            rsync_args: Vec::new(),
        }
    }
}

/// Every key `CollectionConfig` reads.
const COLLECTION_KEYS: [&str; 7] = [
    "id",
    "patterns",
    "exclude",
    "ignore",
    "timeless",
    "idle_days",
    "soon_days",
];

/// Every key `TargetConfig` reads.
const TARGET_KEYS: [&str; 5] = ["url", "rsync", "rsync_path", "fsync", "rsync_args"];

/// The `[collection]` keys of the earlier design, in the order the spec
/// lists them.
const REMOVED_COLLECTION_KEYS: [&str; 11] = [
    "junk",
    "remote_only_days",
    "backup_live",
    "live_exclude",
    "replace_without_asking",
    "brake_percent",
    "grace_hours",
    "away_warn_days",
    "memory_percent",
    "remote_jobs",
    "live_retries",
];

/// The defaults of the config table in the spec.
mod defaults {
    pub fn patterns() -> Vec<String> {
        vec!["*".to_string()]
    }
    pub fn ignore() -> Vec<String> {
        vec![".DS_Store".to_string()]
    }
    pub fn idle_days() -> u32 {
        30
    }
    pub fn soon_days() -> u32 {
        3
    }
}

/// The settings of the earlier design that `table` still holds, named the
/// way the spec names them. A setting counts as present by its key alone,
/// whatever the value.
fn removed_keys(table: &toml::Table) -> Vec<String> {
    let mut found = Vec::new();
    if let Some(collection) = table.get("collection").and_then(toml::Value::as_table) {
        found.extend(
            REMOVED_COLLECTION_KEYS
                .iter()
                .filter(|key| collection.contains_key(**key))
                .map(|key| format!("collection.{key}")),
        );
    }
    if table.contains_key("machine") {
        found.push("[machine]".to_string());
    }
    if table.contains_key("remote") {
        found.push("[[remote]]".to_string());
    }
    found
}

/// The keys of the table `name` in `table` that are not in `known`.
fn unknown_keys(table: &toml::Table, name: &str, known: &[&str]) -> Vec<String> {
    table
        .get(name)
        .and_then(toml::Value::as_table)
        .map(|section| {
            section
                .keys()
                .filter(|key| !known.contains(&key.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

impl Config {
    /// A config with every setting at its default.
    pub fn new(id: ObjectId) -> Config {
        Config {
            collection: CollectionConfig {
                id,
                patterns: defaults::patterns(),
                exclude: Vec::new(),
                ignore: defaults::ignore(),
                timeless: Vec::new(),
                idle_days: defaults::idle_days(),
                soon_days: defaults::soon_days(),
            },
            target: TargetConfig::default(),
            unknown_collection_keys: Vec::new(),
            unknown_target_keys: Vec::new(),
        }
    }

    /// Parses `text`. `path` only names the file in error messages.
    pub fn parse(text: &str, path: Option<&Path>) -> Result<Config, ConfigError> {
        let parse_error = |source| ConfigError::Parse {
            path: path.map(Path::to_path_buf),
            source,
        };
        // The plain table is read first: the typed read below drops unknown
        // keys, and a removed setting has to be refused before a type error
        // in it could hide the refusal.
        let table: toml::Table = toml::from_str(text).map_err(parse_error)?;
        let removed = removed_keys(&table);
        if !removed.is_empty() {
            return Err(ConfigError::RemovedKeys {
                path: path.map(Path::to_path_buf),
                keys: removed,
            });
        }
        let mut config: Config = toml::from_str(text).map_err(parse_error)?;
        config.unknown_collection_keys = unknown_keys(&table, "collection", &COLLECTION_KEYS);
        config.unknown_target_keys = unknown_keys(&table, "target", &TARGET_KEYS);
        Ok(config)
    }

    pub fn read(path: &Path) -> Result<Config, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Config::parse(&text, Some(path))
    }
}

/// A config file open for editing. Edits keep the comments, blank lines
/// and key order of everything they do not touch.
#[derive(Debug, Clone)]
pub struct ConfigDoc {
    doc: DocumentMut,
    path: Option<PathBuf>,
}

impl ConfigDoc {
    /// The document `init` writes. `id` and the patterns given on the
    /// command line are active lines. Every other `[collection]` key is
    /// shown commented out with its default, so that a later change of a
    /// default still reaches this collection. The `[target]` block is
    /// commented out as a whole.
    pub fn new(id: ObjectId, given_patterns: &[String]) -> ConfigDoc {
        let array = |items: Vec<String>| Array::from_iter(items).to_string();
        let patterns = if given_patterns.is_empty() {
            format!("# patterns = {}", array(defaults::patterns()))
        } else {
            format!("patterns = {}", array(given_patterns.to_vec()))
        };
        let ignore = array(defaults::ignore());
        let idle_days = defaults::idle_days();
        let soon_days = defaults::soon_days();
        let text = format!(
            r#"# antiq collection configuration.

[collection]
# ID of this collection. The marker `push` writes to the target records it.
id = "{id}"

# Glob patterns that select the pieces, relative to this directory. When
# matches nest, the shallowest one is the piece. A folder whose name starts
# with a dot is a piece only if a pattern names the dot.
{patterns}

# Paths that are never pieces: literal paths relative to this directory,
# not patterns. A `%` is written `%25` and a byte that is not UTF-8 `%XX`.
# exclude = []

# gitignore lines for paths that are not packed, not listed and not counted
# for the idle check. `archive` deletes them with the folder. A line without
# a `/` (except at its end) matches at any depth below the top of a piece; a
# `/` at the start or in the middle anchors it at that top.
# ignore = {ignore}

# gitignore lines for paths that are packed but not counted for the idle
# check, such as ".git/". The lines follow the same rules as `ignore`.
# timeless = []

# Days a piece must be idle before it is due.
# idle_days = {idle_days}

# Days before a piece is due from which `status` shows it as soon.
# soon_days = {soon_days}

# Where `push` mirrors the collection. Remove the `#` from the lines you
# need; `url` is the only one `push` requires.
#
# [target]
# An absolute path, or [user@]host:path for an ssh target, for example
# "backup@nas.local:/volume1/antiq/projects".
# url = "/Volumes/nas/antiq/projects"
# The local rsync. Without it, rsync from PATH.
# rsync = "/opt/homebrew/bin/rsync"
# The rsync on the target, passed as --rsync-path.
# rsync_path = "/usr/local/bin/rsync"
# Set to false when the rsync on the target lacks --fsync.
# fsync = false
# Extra rsync options, one string each. An option here overrides antiq's own,
# such as --no-perms after -a.
# rsync_args = ["--no-perms"]
"#
        );
        ConfigDoc::parse(&text, None).expect("the template is valid TOML")
    }

    /// Parses `text`. `path` only names the file in error messages.
    pub fn parse(text: &str, path: Option<&Path>) -> Result<ConfigDoc, ConfigError> {
        let path = path.map(Path::to_path_buf);
        let doc = text
            .parse::<DocumentMut>()
            .map_err(|source| ConfigError::Edit {
                path: path.clone(),
                source,
            })?;
        Ok(ConfigDoc { doc, path })
    }

    pub fn read(path: &Path) -> Result<ConfigDoc, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        ConfigDoc::parse(&text, Some(path))
    }

    /// The text to write to the file. It is handed out only when the config
    /// reader accepts it, so no edit can leave a file that every later
    /// command refuses.
    pub fn render(&self) -> Result<String, ConfigError> {
        let text = self.doc.to_string();
        Config::parse(&text, self.path.as_deref())?;
        Ok(text)
    }

    pub fn config(&self) -> Result<Config, ConfigError> {
        Config::parse(&self.doc.to_string(), self.path.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::COLLECTION_ID;

    const MINIMAL: &str = "[collection]\nid = \"01ARZ3NDEKTSV4RRFFQ69G5FAV\"\n";

    fn parse(text: &str) -> Config {
        Config::parse(text, None).expect("the config parses")
    }

    fn error_text(text: &str) -> String {
        Config::parse(text, None)
            .expect_err("the config is refused")
            .to_string()
    }

    fn with_collection_line(line: &str) -> String {
        format!("{MINIMAL}{line}\n")
    }

    fn with_target_line(line: &str) -> String {
        format!("{MINIMAL}[target]\n{line}\n")
    }

    #[test]
    fn defaults_equal_the_spec_table() {
        let config = parse(MINIMAL);
        let collection = &config.collection;
        assert_eq!(collection.id, COLLECTION_ID.parse().expect("a ULID"));
        assert_eq!(collection.patterns, vec!["*".to_string()]);
        assert_eq!(collection.exclude, Vec::<RelPath>::new());
        assert_eq!(collection.ignore, vec![".DS_Store".to_string()]);
        assert_eq!(collection.timeless, Vec::<String>::new());
        assert_eq!(collection.idle_days, 30);
        assert_eq!(collection.soon_days, 3);
    }

    #[test]
    fn the_target_defaults_equal_the_spec_table() {
        let target = parse(MINIMAL).target;
        assert_eq!(target.url, None);
        assert_eq!(target.rsync, None);
        assert_eq!(target.rsync_path, None);
        assert!(target.fsync);
        assert_eq!(target.rsync_args, Vec::<String>::new());
    }

    #[test]
    fn new_config_equals_a_parsed_minimal_one() {
        assert_eq!(
            Config::new(COLLECTION_ID.parse().expect("a ULID")),
            parse(MINIMAL)
        );
    }

    #[test]
    fn every_key_can_be_overridden() {
        let config = parse(&format!(
            r#"
[collection]
id = "{COLLECTION_ID}"
patterns = ["projects/*"]
exclude = ["archive", "a%FFb"]
ignore = ["target/"]
timeless = ["*.iso"]
idle_days = 1
soon_days = 2

[target]
url = "nas.local:/volume1/antiq"
rsync = "/opt/homebrew/bin/rsync"
rsync_path = "/usr/local/bin/rsync"
fsync = false
rsync_args = ["--bwlimit=100", "--no-perms"]
"#
        ));
        let collection = &config.collection;
        assert_eq!(collection.patterns, vec!["projects/*".to_string()]);
        assert_eq!(
            collection.exclude,
            vec![
                RelPath::parse("archive").expect("valid"),
                RelPath::from_bytes(b"a\xFFb".to_vec()).expect("valid"),
            ]
        );
        assert_eq!(collection.ignore, vec!["target/".to_string()]);
        assert_eq!(collection.timeless, vec!["*.iso".to_string()]);
        assert_eq!(collection.idle_days, 1);
        assert_eq!(collection.soon_days, 2);
        let target = &config.target;
        assert_eq!(target.url.as_deref(), Some("nas.local:/volume1/antiq"));
        assert_eq!(
            target.rsync.as_deref(),
            Some(Path::new("/opt/homebrew/bin/rsync"))
        );
        assert_eq!(target.rsync_path.as_deref(), Some("/usr/local/bin/rsync"));
        assert!(!target.fsync);
        assert_eq!(target.rsync_args, ["--bwlimit=100", "--no-perms"]);
        assert!(config.unknown_collection_keys.is_empty());
        assert!(config.unknown_target_keys.is_empty());
    }

    #[test]
    fn a_target_url_is_kept_as_written() {
        let config = parse(&with_target_line("url = \"not a url at all\""));
        assert_eq!(config.target.url.as_deref(), Some("not a url at all"));
    }

    // =====================================================================
    // Keys of the earlier design
    // =====================================================================

    const REMOVED_COLLECTION_LINES: [&str; 11] = [
        "junk = [\"Thumbs.db\"]",
        "remote_only_days = 180",
        "backup_live = true",
        "live_exclude = [\"cache\"]",
        "replace_without_asking = true",
        "brake_percent = 20",
        "grace_hours = 24",
        "away_warn_days = 7",
        "memory_percent = 25",
        "remote_jobs = 4",
        "live_retries = 3",
    ];

    #[test]
    fn each_removed_collection_key_is_refused_naming_it() {
        for line in REMOVED_COLLECTION_LINES {
            let key = line.split(' ').next().expect("a key");
            let error = Config::parse(&with_collection_line(line), None)
                .expect_err("a removed key is refused");
            assert!(
                matches!(&error, ConfigError::RemovedKeys { keys, .. } if *keys == [format!("collection.{key}")]),
                "{key}: {error}"
            );
        }
    }

    #[test]
    fn the_machine_table_and_the_remote_list_are_refused() {
        assert_eq!(
            error_text(&format!("{MINIMAL}[machine]\nname = \"laptop\"\n")),
            "invalid config: these settings no longer exist: [machine]"
        );
        assert_eq!(
            error_text(&format!("{MINIMAL}[[remote]]\nurl = \"/srv\"\n")),
            "invalid config: these settings no longer exist: [[remote]]"
        );
    }

    #[test]
    fn every_removed_setting_is_named_in_one_error_in_the_order_of_the_spec() {
        let lines = REMOVED_COLLECTION_LINES.join("\n");
        let text = format!("{MINIMAL}{lines}\n[machine]\n[[remote]]\n");
        assert_eq!(
            error_text(&text),
            "invalid config: these settings no longer exist: collection.junk, \
             collection.remote_only_days, collection.backup_live, collection.live_exclude, \
             collection.replace_without_asking, collection.brake_percent, \
             collection.grace_hours, collection.away_warn_days, collection.memory_percent, \
             collection.remote_jobs, collection.live_retries, [machine], [[remote]]"
        );
    }

    #[test]
    fn a_removed_key_is_refused_by_its_presence_whatever_its_value() {
        assert!(error_text(&format!("remote = []\n{MINIMAL}")).contains("[[remote]]"));
        assert!(error_text(&format!("machine = {{}}\n{MINIMAL}")).contains("[machine]"));
        assert!(error_text(&format!("{MINIMAL}[machine]\n")).contains("[machine]"));
    }

    #[test]
    fn a_removed_setting_is_refused_before_a_missing_collection_id_is_noticed() {
        let error = Config::parse("[machine]\nname = \"laptop\"\n", None).expect_err("refused");
        assert!(matches!(error, ConfigError::RemovedKeys { .. }), "{error}");
    }

    #[test]
    fn a_removed_setting_is_refused_before_a_type_error_in_another_key() {
        let error = error_text(&with_collection_line("junk = []\nidle_days = \"soon\""));
        assert!(error.contains("collection.junk"), "{error}");
    }

    #[test]
    fn a_collection_that_is_no_table_does_not_hide_the_refusal_or_panic() {
        let error = error_text("collection = 5\n[machine]\n");
        assert!(error.contains("[machine]"), "{error}");
    }

    #[test]
    fn a_collection_that_is_no_table_fails_the_typed_read() {
        assert!(error_text("collection = 5\n").contains("collection"));
    }

    // =====================================================================
    // Unknown keys
    // =====================================================================

    #[test]
    fn unknown_collection_key_is_ignored_and_listed() {
        let config = parse(&with_collection_line("idle_dyas = 1"));
        assert_eq!(config.collection, parse(MINIMAL).collection);
        assert_eq!(
            config.unknown_collection_keys,
            vec!["idle_dyas".to_string()]
        );
    }

    #[test]
    fn every_unknown_collection_key_is_listed() {
        let config = parse(&with_collection_line("one = 1\ntwo = 2"));
        assert_eq!(
            config.unknown_collection_keys,
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn unknown_target_keys_are_ignored_and_listed() {
        let config = parse(&with_target_line("rsynk = \"x\"\nfsync = false\nzzz = 1"));
        assert_eq!(config.target.rsync, None);
        assert!(!config.target.fsync);
        assert_eq!(
            config.unknown_target_keys,
            vec!["rsynk".to_string(), "zzz".to_string()]
        );
        assert!(config.unknown_collection_keys.is_empty());
    }

    #[test]
    fn unknown_top_level_table_is_ignored() {
        let config = parse(&format!("{MINIMAL}[future]\nsetting = 1\n"));
        assert_eq!(config, parse(MINIMAL));
    }

    // =====================================================================
    // Values
    // =====================================================================

    #[test]
    fn a_missing_collection_id_fails() {
        assert!(error_text("[collection]\nidle_days = 3\n").contains("id"));
    }

    #[test]
    fn an_invalid_collection_id_fails() {
        assert!(error_text("[collection]\nid = \"not-a-ulid\"\n").contains("not-a-ulid"));
    }

    #[test]
    fn a_type_error_carries_the_key_in_the_source_snippet() {
        assert!(error_text(&with_collection_line("idle_days = \"soon\"")).contains("idle_days"));
    }

    #[test]
    fn a_type_error_in_soon_days_names_the_key() {
        assert!(error_text(&with_collection_line("soon_days = -1")).contains("soon_days"));
    }

    #[test]
    fn type_errors_in_the_target_name_the_key() {
        assert!(error_text(&with_target_line("fsync = \"yes\"")).contains("fsync"));
        assert!(error_text(&with_target_line("rsync_args = \"-v\"")).contains("rsync_args"));
    }

    #[test]
    fn an_exclude_entry_is_read_in_the_escaped_form() {
        let config = parse(&with_collection_line("exclude = [\"a%FFb\", \"Ünï\"]"));
        let bytes: Vec<&[u8]> = config
            .collection
            .exclude
            .iter()
            .map(RelPath::as_bytes)
            .collect();
        assert_eq!(bytes, vec![&b"a\xFFb"[..], "Ünï".as_bytes()]);
    }

    #[test]
    fn an_exclude_entry_with_a_bad_escape_fails_naming_the_key() {
        let text = error_text(&with_collection_line("exclude = [\"a%ZZ\"]"));
        assert!(text.contains("exclude"), "{text}");
    }

    #[test]
    fn reading_a_missing_file_reports_its_path() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("config.toml");
        let error = Config::read(&path).expect_err("there is no file");
        assert!(matches!(error, ConfigError::Read { .. }), "{error}");
        assert!(error.to_string().contains("config.toml"), "{error}");
    }

    #[test]
    fn reading_a_broken_file_reports_its_path() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("config.toml");
        fs::write(&path, "[collection\n").expect("writable");
        let error = Config::read(&path).expect_err("the file is broken");
        assert!(error.to_string().contains("config.toml"), "{error}");
    }

    #[test]
    fn a_refusal_of_removed_settings_names_the_file() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("config.toml");
        fs::write(&path, format!("{MINIMAL}[machine]\n")).expect("writable");
        let error = Config::read(&path).expect_err("a removed table");
        assert!(error.to_string().contains("config.toml"), "{error}");
    }

    #[test]
    fn reading_a_good_file_returns_the_config() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("config.toml");
        fs::write(&path, MINIMAL).expect("writable");
        assert_eq!(Config::read(&path).expect("readable"), parse(MINIMAL));
    }

    // =====================================================================
    // ConfigDoc
    // =====================================================================

    const COMMENTED: &str = r#"# antiq config of the playground
[collection]
id = "01ARZ3NDEKTSV4RRFFQ69G5FAV" # never change this
idle_days = 30
"#;

    fn doc(text: &str) -> ConfigDoc {
        ConfigDoc::parse(text, None).expect("the document parses")
    }

    fn collection_id() -> ObjectId {
        COLLECTION_ID.parse().expect("a ULID")
    }

    fn written(patterns: &[&str]) -> String {
        let given: Vec<String> = patterns.iter().map(ToString::to_string).collect();
        ConfigDoc::new(collection_id(), &given)
            .render()
            .expect("the written document is a valid config")
    }

    /// The line of `key` in a written document, active or commented out.
    fn line_of(text: &str, key: &str) -> Option<usize> {
        text.lines().position(|line| {
            let line = line.strip_prefix("# ").unwrap_or(line);
            line.starts_with(&format!("{key} = "))
        })
    }

    #[test]
    fn a_new_document_reads_back_as_the_defaults() {
        let config = Config::parse(&written(&[]), None).expect("the document parses");
        assert_eq!(config, Config::new(collection_id()));
    }

    #[test]
    fn given_patterns_are_written_as_an_active_line() {
        let text = written(&["projects/*", ".*"]);
        assert!(
            text.lines()
                .any(|line| line == r#"patterns = ["projects/*", ".*"]"#),
            "{text}"
        );
        let config = Config::parse(&text, None).expect("the document parses");
        let mut expected = Config::new(collection_id());
        expected.collection.patterns = vec!["projects/*".into(), ".*".into()];
        assert_eq!(config, expected);
    }

    #[test]
    fn patterns_with_quotes_and_backslashes_read_back() {
        let text = written(&["a\"b", "c\\d"]);
        let config = Config::parse(&text, None).expect("the document parses");
        assert_eq!(config.collection.patterns, ["a\"b", "c\\d"]);
    }

    #[test]
    fn only_what_init_sets_is_an_active_line() {
        let active = |text: &str| -> Vec<String> {
            let table: toml::Table = toml::from_str(text).expect("valid TOML");
            table["collection"]
                .as_table()
                .expect("a table")
                .keys()
                .cloned()
                .collect()
        };
        assert_eq!(active(&written(&[])), ["id"]);
        assert_eq!(active(&written(&["a*"])), ["id", "patterns"]);
    }

    #[test]
    fn every_collection_key_has_a_comment() {
        let text = written(&[]);
        let lines: Vec<&str> = text.lines().collect();
        for key in COLLECTION_KEYS {
            let index = line_of(&text, key).unwrap_or_else(|| panic!("{key} has no line"));
            let above = lines[index.checked_sub(1).expect("a comment comes first")];
            let key_line = |line: &str| {
                COLLECTION_KEYS.iter().any(|other| {
                    line.trim_start_matches("# ")
                        .starts_with(&format!("{other} = "))
                })
            };
            assert!(
                above.starts_with("# ") && !key_line(above),
                "{key} has no comment above it: {above:?}"
            );
        }
    }

    #[test]
    fn the_target_block_is_commented_out_with_every_key() {
        let text = written(&[]);
        assert!(text.lines().any(|line| line == "# [target]"), "{text}");
        assert!(!text.lines().any(|line| line == "[target]"), "{text}");
        for key in TARGET_KEYS {
            assert!(
                text.lines()
                    .any(|line| line.starts_with(&format!("# {key} = "))),
                "{key} has no example"
            );
        }
    }

    #[test]
    fn the_target_examples_work_when_uncommented() {
        let text = written(&[]);
        let block = text
            .split_once("# [target]\n")
            .expect("the target block exists")
            .1;
        let examples: Vec<&str> = block
            .lines()
            .filter_map(|line| line.strip_prefix("# "))
            .filter(|line| {
                TARGET_KEYS
                    .iter()
                    .any(|key| line.starts_with(&format!("{key} = ")))
            })
            .collect();
        assert_eq!(examples.len(), TARGET_KEYS.len(), "{block}");
        let config = Config::parse(&format!("{text}[target]\n{}\n", examples.join("\n")), None)
            .expect("the uncommented block parses");
        assert!(config.unknown_target_keys.is_empty());
        let url = config.target.url.expect("an example url");
        crate::target::url::TargetUrl::parse(&url).expect("the url example is valid");
        assert!(config.target.rsync.is_some());
        assert!(config.target.rsync_path.is_some());
        assert!(!config.target.fsync);
        assert_eq!(config.target.rsync_args, ["--no-perms"]);
    }

    #[test]
    fn the_ssh_example_in_the_comment_is_a_valid_url() {
        let text = written(&[]);
        let comment = text
            .lines()
            .skip_while(|line| *line != "# [target]")
            .take_while(|line| !line.starts_with("# url = "))
            .collect::<Vec<_>>()
            .join("\n");
        let example = comment
            .split('"')
            .nth(1)
            .expect("the comment quotes an ssh url");
        assert!(example.contains('@'), "{example}");
        crate::target::url::TargetUrl::parse(example).expect("the ssh example is valid");
    }

    #[test]
    fn rendering_an_untouched_document_returns_the_text_as_read() {
        assert_eq!(doc(COMMENTED).render().expect("renders"), COMMENTED);
    }

    #[test]
    fn rendering_refuses_a_document_with_a_removed_setting() {
        let document = doc(&format!("{MINIMAL}[machine]\n"));
        let error = document.render().expect_err("a removed table");
        assert!(matches!(error, ConfigError::RemovedKeys { .. }), "{error}");
    }

    #[test]
    fn parsing_a_document_with_a_syntax_error_names_the_file() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("config.toml");
        fs::write(&path, "[collection\n").expect("writable");
        let error = ConfigDoc::read(&path).expect_err("the file is broken");
        assert!(matches!(error, ConfigError::Edit { .. }), "{error}");
        assert!(error.to_string().contains("config.toml"), "{error}");
    }

    #[test]
    fn reading_a_document_that_is_missing_names_the_file() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("config.toml");
        let error = ConfigDoc::read(&path).expect_err("there is no file");
        assert!(matches!(error, ConfigError::Read { .. }), "{error}");
    }

    #[test]
    fn reading_a_document_keeps_its_text() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("config.toml");
        fs::write(&path, COMMENTED).expect("writable");
        let document = ConfigDoc::read(&path).expect("readable");
        assert_eq!(document.render().expect("renders"), COMMENTED);
    }
}

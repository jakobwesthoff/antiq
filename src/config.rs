// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The collection's config file.
//!
//! `[collection]` holds the policy and is read leniently. `[machine]` and
//! every `[[remote]]` entry are local to the machine and read strictly, so
//! a misspelled key there fails the run instead of silently doing nothing.

use crate::id::ObjectId;
use crate::relpath::RelPath;
use serde::{Deserialize, Deserializer};
use std::fs;
use std::path::{Path, PathBuf};

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
    #[error("version 1 accepts exactly one [[remote]] entry")]
    SecondRemote,
    #[error("{key} must be {min} to {max}, found {value}")]
    OutOfRange {
        key: &'static str,
        value: u32,
        min: u32,
        max: u32,
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
    pub machine: MachineConfig,
    /// The `[[remote]]` list. Version 1 accepts at most one entry.
    #[serde(default, rename = "remote")]
    pub remotes: Vec<RemoteConfig>,
    /// Keys of `[collection]` that this version does not know. They are
    /// ignored, so that a config written by a newer version still reads;
    /// callers warn about each one, since a misspelled policy key would
    /// otherwise pass unnoticed.
    #[serde(skip)]
    pub unknown_collection_keys: Vec<String>,
}

/// The `[collection]` table. Every key but `id` has a default.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CollectionConfig {
    pub id: ObjectId,
    #[serde(default = "defaults::patterns")]
    pub patterns: Vec<String>,
    /// Literal paths that are never pieces, in the escaped form of the
    /// metadata.
    #[serde(default)]
    pub exclude: Vec<RelPath>,
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default = "defaults::junk")]
    pub junk: Vec<String>,
    #[serde(default = "defaults::idle_days")]
    pub idle_days: u32,
    #[serde(default = "defaults::remote_only_days")]
    pub remote_only_days: u32,
    #[serde(default = "defaults::backup_live")]
    pub backup_live: bool,
    #[serde(default)]
    pub live_exclude: Vec<String>,
    #[serde(default)]
    pub replace_without_asking: bool,
    #[serde(default = "defaults::brake_percent")]
    pub brake_percent: u32,
    #[serde(default = "defaults::grace_hours")]
    pub grace_hours: u32,
    #[serde(default = "defaults::away_warn_days")]
    pub away_warn_days: u32,
    /// Share of the RAM that compression may use, 1 to 100.
    #[serde(
        default = "defaults::memory_percent",
        deserialize_with = "memory_percent"
    )]
    pub memory_percent: u8,
    #[serde(default = "defaults::remote_jobs")]
    pub remote_jobs: u32,
    #[serde(default = "defaults::live_retries")]
    pub live_retries: u32,
}

/// Every key `CollectionConfig` reads.
const COLLECTION_KEYS: [&str; 16] = [
    "id",
    "patterns",
    "exclude",
    "ignore",
    "junk",
    "idle_days",
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
    pub fn junk() -> Vec<String> {
        vec![".DS_Store".to_string()]
    }
    pub fn idle_days() -> u32 {
        14
    }
    pub fn remote_only_days() -> u32 {
        180
    }
    pub fn backup_live() -> bool {
        true
    }
    pub fn brake_percent() -> u32 {
        20
    }
    pub fn grace_hours() -> u32 {
        24
    }
    pub fn away_warn_days() -> u32 {
        7
    }
    pub fn memory_percent() -> u8 {
        25
    }
    pub fn remote_jobs() -> u32 {
        4
    }
    pub fn live_retries() -> u32 {
        3
    }
}

/// Reads the integer as an `i64` so that every integer, not only the ones
/// that fit the field, reaches the range check and the error names the key.
fn memory_percent<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    let value = i64::deserialize(deserializer)?;
    u8::try_from(value)
        .ok()
        .filter(|percent| (1..=100).contains(percent))
        .ok_or_else(|| {
            serde::de::Error::custom(format!(
                "collection.memory_percent must be 1 to 100, found {value}"
            ))
        })
}

/// The `[machine]` table: settings of this machine, never uploaded.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MachineConfig {
    /// `None` until `init` stores the detected name.
    pub name: Option<String>,
    pub rsync: Option<PathBuf>,
    /// Passed to rsync as `--rsync-path`.
    pub remote_rsync: Option<String>,
    pub connect_timeout: u32,
    pub server_alive_interval: u32,
    pub server_alive_count: u32,
    pub control_persist: u32,
    pub rsync_timeout: u32,
}

impl Default for MachineConfig {
    fn default() -> Self {
        MachineConfig {
            name: None,
            rsync: None,
            remote_rsync: None,
            connect_timeout: 30,
            server_alive_interval: 15,
            server_alive_count: 4,
            control_persist: 60,
            rsync_timeout: 300,
        }
    }
}

/// One `[[remote]]` entry.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    /// As given to `remote add`.
    pub url: String,
    /// The remote's ULID.
    pub id: ObjectId,
    pub host_key_alias: Option<String>,
}

impl Config {
    /// A config with every setting at its default.
    pub fn new(id: ObjectId) -> Config {
        Config {
            collection: CollectionConfig {
                id,
                patterns: defaults::patterns(),
                exclude: Vec::new(),
                ignore: Vec::new(),
                junk: defaults::junk(),
                idle_days: defaults::idle_days(),
                remote_only_days: defaults::remote_only_days(),
                backup_live: defaults::backup_live(),
                live_exclude: Vec::new(),
                replace_without_asking: false,
                brake_percent: defaults::brake_percent(),
                grace_hours: defaults::grace_hours(),
                away_warn_days: defaults::away_warn_days(),
                memory_percent: defaults::memory_percent(),
                remote_jobs: defaults::remote_jobs(),
                live_retries: defaults::live_retries(),
            },
            machine: MachineConfig::default(),
            remotes: Vec::new(),
            unknown_collection_keys: Vec::new(),
        }
    }

    /// Parses `text`. `path` only names the file in error messages.
    pub fn parse(text: &str, path: Option<&Path>) -> Result<Config, ConfigError> {
        let parse_error = |source| ConfigError::Parse {
            path: path.map(Path::to_path_buf),
            source,
        };
        let mut config: Config = toml::from_str(text).map_err(parse_error)?;
        if config.remotes.len() > 1 {
            return Err(ConfigError::SecondRemote);
        }
        config.check_ranges()?;

        // The typed read has dropped unknown `[collection]` keys, so they
        // are found in the plain table.
        let table: toml::Table = toml::from_str(text).map_err(parse_error)?;
        let collection = table["collection"]
            .as_table()
            .expect("the typed read has required a [collection] table");
        config.unknown_collection_keys = collection
            .keys()
            .filter(|key| !COLLECTION_KEYS.contains(&key.as_str()))
            .cloned()
            .collect();
        Ok(config)
    }

    /// Refuses the values that would switch a setting off or make it
    /// meaningless. `memory_percent` is checked while it is read.
    fn check_ranges(&self) -> Result<(), ConfigError> {
        let collection = &self.collection;
        let machine = &self.machine;
        let ranged = [
            ("collection.brake_percent", collection.brake_percent, 0, 100),
            (
                "collection.remote_jobs",
                collection.remote_jobs,
                1,
                u32::MAX,
            ),
            (
                "collection.live_retries",
                collection.live_retries,
                1,
                u32::MAX,
            ),
            (
                "machine.connect_timeout",
                machine.connect_timeout,
                1,
                u32::MAX,
            ),
            (
                "machine.server_alive_interval",
                machine.server_alive_interval,
                1,
                u32::MAX,
            ),
            (
                "machine.server_alive_count",
                machine.server_alive_count,
                1,
                u32::MAX,
            ),
            (
                "machine.control_persist",
                machine.control_persist,
                1,
                u32::MAX,
            ),
            ("machine.rsync_timeout", machine.rsync_timeout, 1, u32::MAX),
        ];
        for (key, value, min, max) in ranged {
            if !(min..=max).contains(&value) {
                return Err(ConfigError::OutOfRange {
                    key,
                    value,
                    min,
                    max,
                });
            }
        }
        Ok(())
    }

    pub fn read(path: &Path) -> Result<Config, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Config::parse(&text, Some(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::COLLECTION_ID;

    const MINIMAL: &str = "[collection]\nid = \"01ARZ3NDEKTSV4RRFFQ69G5FAV\"\n";
    const REMOTE_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";

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

    #[test]
    fn defaults_equal_the_spec_table() {
        let config = parse(MINIMAL);
        let collection = &config.collection;
        assert_eq!(collection.id, COLLECTION_ID.parse().expect("a ULID"));
        assert_eq!(collection.patterns, vec!["*".to_string()]);
        assert_eq!(collection.exclude, Vec::<RelPath>::new());
        assert_eq!(collection.ignore, Vec::<String>::new());
        assert_eq!(collection.junk, vec![".DS_Store".to_string()]);
        assert_eq!(collection.idle_days, 14);
        assert_eq!(collection.remote_only_days, 180);
        assert!(collection.backup_live);
        assert_eq!(collection.live_exclude, Vec::<String>::new());
        assert!(!collection.replace_without_asking);
        assert_eq!(collection.brake_percent, 20);
        assert_eq!(collection.grace_hours, 24);
        assert_eq!(collection.away_warn_days, 7);
        assert_eq!(collection.memory_percent, 25);
        assert_eq!(collection.remote_jobs, 4);
        assert_eq!(collection.live_retries, 3);
        let machine = &config.machine;
        assert_eq!(machine.name, None);
        assert_eq!(machine.rsync, None);
        assert_eq!(machine.remote_rsync, None);
        assert_eq!(machine.connect_timeout, 30);
        assert_eq!(machine.server_alive_interval, 15);
        assert_eq!(machine.server_alive_count, 4);
        assert_eq!(machine.control_persist, 60);
        assert_eq!(machine.rsync_timeout, 300);
        assert!(config.remotes.is_empty());
    }

    #[test]
    fn new_config_equals_a_parsed_minimal_one() {
        assert_eq!(
            Config::new(COLLECTION_ID.parse().expect("a ULID")),
            parse(MINIMAL)
        );
    }

    fn every_key_text() -> String {
        format!(
            r#"
[collection]
id = "{COLLECTION_ID}"
patterns = ["projects/*"]
exclude = ["archive", "a%FFb"]
ignore = ["target/"]
junk = ["Thumbs.db"]
idle_days = 1
remote_only_days = 2
backup_live = false
live_exclude = ["cache"]
replace_without_asking = true
brake_percent = 3
grace_hours = 4
away_warn_days = 5
memory_percent = 6
remote_jobs = 7
live_retries = 8

[machine]
name = "laptop"
rsync = "/opt/homebrew/bin/rsync"
remote_rsync = "/usr/bin/rsync"
connect_timeout = 9
server_alive_interval = 10
server_alive_count = 11
control_persist = 12
rsync_timeout = 13

[[remote]]
url = "ssh://nas/volume1/antiq"
id = "{REMOTE_ID}"
host_key_alias = "nas"
"#
        )
    }

    #[test]
    fn every_key_can_be_overridden() {
        let config = parse(&every_key_text());
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
        assert_eq!(collection.junk, vec!["Thumbs.db".to_string()]);
        assert_eq!(collection.idle_days, 1);
        assert_eq!(collection.remote_only_days, 2);
        assert!(!collection.backup_live);
        assert_eq!(collection.live_exclude, vec!["cache".to_string()]);
        assert!(collection.replace_without_asking);
        assert_eq!(collection.brake_percent, 3);
        assert_eq!(collection.grace_hours, 4);
        assert_eq!(collection.away_warn_days, 5);
        assert_eq!(collection.memory_percent, 6);
        assert_eq!(collection.remote_jobs, 7);
        assert_eq!(collection.live_retries, 8);
        let machine = &config.machine;
        assert_eq!(machine.name.as_deref(), Some("laptop"));
        assert_eq!(
            machine.rsync.as_deref(),
            Some(Path::new("/opt/homebrew/bin/rsync"))
        );
        assert_eq!(machine.remote_rsync.as_deref(), Some("/usr/bin/rsync"));
        assert_eq!(machine.connect_timeout, 9);
        assert_eq!(machine.server_alive_interval, 10);
        assert_eq!(machine.server_alive_count, 11);
        assert_eq!(machine.control_persist, 12);
        assert_eq!(machine.rsync_timeout, 13);
        assert_eq!(config.remotes.len(), 1);
        assert_eq!(config.remotes[0].url, "ssh://nas/volume1/antiq");
        assert_eq!(config.remotes[0].id.to_string(), REMOTE_ID);
        assert_eq!(config.remotes[0].host_key_alias.as_deref(), Some("nas"));
    }

    #[test]
    fn unknown_machine_key_network_check_fails_naming_it() {
        let text = format!("{MINIMAL}[machine]\nnetwork_check = true\n");
        assert!(error_text(&text).contains("network_check"));
    }

    #[test]
    fn unknown_machine_key_typo_fails_naming_it() {
        let text = format!("{MINIMAL}[machine]\nrsyc = \"/usr/bin/rsync\"\n");
        assert!(error_text(&text).contains("rsyc"));
    }

    #[test]
    fn unknown_remote_key_fails_naming_it() {
        let text = format!(
            "{MINIMAL}[[remote]]\nurl = \"/srv\"\nid = \"{REMOTE_ID}\"\nhost_key_alais = \"x\"\n"
        );
        assert!(error_text(&text).contains("host_key_alais"));
    }

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
    fn known_keys_are_not_listed_as_unknown() {
        assert!(parse(&every_key_text()).unknown_collection_keys.is_empty());
    }

    #[test]
    fn unknown_top_level_table_is_ignored() {
        let config = parse(&format!("{MINIMAL}[future]\nsetting = 1\n"));
        assert_eq!(config, parse(MINIMAL));
    }

    #[test]
    fn one_remote_parses_with_an_alias() {
        let config = parse(&format!(
            "{MINIMAL}[[remote]]\nurl = \"/srv\"\nid = \"{REMOTE_ID}\"\nhost_key_alias = \"nas\"\n"
        ));
        assert_eq!(config.remotes[0].host_key_alias.as_deref(), Some("nas"));
    }

    #[test]
    fn one_remote_parses_without_an_alias() {
        let config = parse(&format!(
            "{MINIMAL}[[remote]]\nurl = \"/srv\"\nid = \"{REMOTE_ID}\"\n"
        ));
        assert_eq!(config.remotes[0].host_key_alias, None);
    }

    #[test]
    fn a_second_remote_fails() {
        let entry = format!("[[remote]]\nurl = \"/srv\"\nid = \"{REMOTE_ID}\"\n");
        let error = Config::parse(&format!("{MINIMAL}{entry}{entry}"), None)
            .expect_err("two remotes are refused");
        assert!(matches!(error, ConfigError::SecondRemote), "{error}");
    }

    fn range_error(table: &str, key: &str, value: &str) -> String {
        let line = format!("{key} = {value}");
        match table {
            "collection" => error_text(&with_collection_line(&line)),
            _ => error_text(&format!("{MINIMAL}[{table}]\n{line}\n")),
        }
    }

    #[test]
    fn brake_percent_101_fails_naming_the_key() {
        let text = range_error("collection", "brake_percent", "101");
        assert!(text.contains("collection.brake_percent"), "{text}");
    }

    #[test]
    fn remote_jobs_0_fails_naming_the_key() {
        let text = range_error("collection", "remote_jobs", "0");
        assert!(text.contains("collection.remote_jobs"), "{text}");
    }

    #[test]
    fn live_retries_0_fails_naming_the_key() {
        let text = range_error("collection", "live_retries", "0");
        assert!(text.contains("collection.live_retries"), "{text}");
    }

    #[test]
    fn connect_timeout_0_fails_naming_the_key() {
        let text = range_error("machine", "connect_timeout", "0");
        assert!(text.contains("machine.connect_timeout"), "{text}");
    }

    #[test]
    fn server_alive_interval_0_fails_naming_the_key() {
        let text = range_error("machine", "server_alive_interval", "0");
        assert!(text.contains("machine.server_alive_interval"), "{text}");
    }

    #[test]
    fn server_alive_count_0_fails_naming_the_key() {
        let text = range_error("machine", "server_alive_count", "0");
        assert!(text.contains("machine.server_alive_count"), "{text}");
    }

    #[test]
    fn control_persist_0_fails_naming_the_key() {
        let text = range_error("machine", "control_persist", "0");
        assert!(text.contains("machine.control_persist"), "{text}");
    }

    #[test]
    fn rsync_timeout_0_fails_naming_the_key() {
        let text = range_error("machine", "rsync_timeout", "0");
        assert!(text.contains("machine.rsync_timeout"), "{text}");
    }

    #[test]
    fn brake_percent_0_passes() {
        let config = parse(&with_collection_line("brake_percent = 0"));
        assert_eq!(config.collection.brake_percent, 0);
    }

    #[test]
    fn brake_percent_100_passes() {
        let config = parse(&with_collection_line("brake_percent = 100"));
        assert_eq!(config.collection.brake_percent, 100);
    }

    #[test]
    fn a_range_error_names_the_key_the_bounds_and_the_value() {
        assert_eq!(
            error_text(&with_collection_line("remote_jobs = 0")),
            "collection.remote_jobs must be 1 to 4294967295, found 0"
        );
    }

    fn memory_percent_error(value: &str) -> String {
        error_text(&with_collection_line(&format!("memory_percent = {value}")))
    }

    #[test]
    fn memory_percent_0_fails_naming_the_key() {
        assert!(memory_percent_error("0").contains("collection.memory_percent"));
    }

    #[test]
    fn memory_percent_101_fails_naming_the_key() {
        assert!(memory_percent_error("101").contains("collection.memory_percent"));
    }

    #[test]
    fn memory_percent_300_fails_naming_the_key() {
        assert!(memory_percent_error("300").contains("collection.memory_percent"));
    }

    #[test]
    fn memory_percent_minus_5_fails_naming_the_key() {
        assert!(memory_percent_error("-5").contains("collection.memory_percent"));
    }

    #[test]
    fn memory_percent_above_u32_fails_naming_the_key() {
        assert!(memory_percent_error("4294967296").contains("collection.memory_percent"));
    }

    #[test]
    fn memory_percent_1_passes() {
        let config = parse(&with_collection_line("memory_percent = 1"));
        assert_eq!(config.collection.memory_percent, 1);
    }

    #[test]
    fn memory_percent_100_passes() {
        let config = parse(&with_collection_line("memory_percent = 100"));
        assert_eq!(config.collection.memory_percent, 100);
    }

    #[test]
    fn a_missing_collection_id_fails() {
        assert!(error_text("[collection]\nidle_days = 3\n").contains("id"));
    }

    #[test]
    fn an_invalid_collection_id_fails() {
        assert!(error_text("[collection]\nid = \"not-a-ulid\"\n").contains("not-a-ulid"));
    }

    #[test]
    fn a_remote_without_a_url_fails() {
        let text = format!("{MINIMAL}[[remote]]\nid = \"{REMOTE_ID}\"\n");
        assert!(error_text(&text).contains("url"));
    }

    #[test]
    fn a_type_error_carries_the_key_in_the_source_snippet() {
        assert!(error_text(&with_collection_line("idle_days = \"soon\"")).contains("idle_days"));
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
    fn reading_a_good_file_returns_the_config() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("config.toml");
        fs::write(&path, MINIMAL).expect("writable");
        assert_eq!(Config::read(&path).expect("readable"), parse(MINIMAL));
    }
}

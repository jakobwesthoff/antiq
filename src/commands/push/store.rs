// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Step 2 of `push`: the store must be whole and the collection must not
//! hold names the target's volume would merge.

use super::PushError;
use crate::archive::TargetRules;
use crate::archive::names::fold_path;
use crate::collection::Collection;
use crate::filesystem::tree::TreeRoot;
use crate::relpath::RelPath;
use crate::report::Warning;
use crate::select::{Rules, select};
use crate::versions::damage::{Check, Condition, Damage, Places, inspect_all};
use crate::versions::store::{StoreVersion, scan};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::io;
use std::path::Path;

/// Refuses while the store holds a damaged or incomplete version, while
/// two live piece paths or two store folders would be one name on the
/// target's volume, and while `trash/` holds a tree. `warnings` are those
/// of the repairs that ran just before.
pub(super) fn check(collection: &Collection, warnings: &[Warning]) -> Result<(), PushError> {
    let store = scan(&collection.pieces_dir()).map_err(PushError::Versions)?;

    let items: Vec<(RelPath, StoreVersion)> = store
        .pieces
        .iter()
        .chain(&store.sidecar_only)
        .flat_map(|piece| {
            piece
                .versions
                .iter()
                .map(|version| (piece.path.clone(), version.clone()))
        })
        .collect();
    let pieces_dir = collection.pieces_dir();
    let tmp_pack_dir = collection.tmp_dir().join("pack");
    let places = Places {
        pieces_dir: &pieces_dir,
        tmp_pack_dir: &tmp_pack_dir,
    };
    // The same width as `grep` and `status`: every core but two, at least
    // one.
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .saturating_sub(2)
        .max(1);
    let inspections = inspect_all(&places, &items, Check::Full, workers);

    let mut lines = Vec::new();
    for ((piece, version), inspection) in items.iter().zip(inspections) {
        let inspection = inspection.map_err(PushError::Versions)?;
        if inspection.condition == Condition::BeingStored {
            lines.push(being_stored(piece, version, warnings));
        } else if let Some(line) = refusal(piece, version, &inspection.condition) {
            lines.push(line);
        }
    }
    if !lines.is_empty() {
        return Err(PushError::DamagedStore { lines });
    }

    let store_folders: Vec<RelPath> = store
        .pieces
        .iter()
        .chain(&store.sidecar_only)
        .map(|piece| piece.path.clone())
        .collect();
    if let Some((first, second)) = find_clash(&store_folders) {
        return Err(PushError::NameClash { first, second });
    }

    let config = &collection.config().collection;
    let root = TreeRoot::open(collection.root()).map_err(|source| PushError::Root {
        path: collection.root().to_path_buf(),
        source,
    })?;
    let rules = Rules::new(&config.patterns, &config.exclude)
        .and_then(|rules| rules.bind(&root))
        .map_err(PushError::Select)?;
    let selection = select(&root, &rules).map_err(PushError::Select)?;
    if let Some((first, second)) = find_clash(selection.pieces()) {
        return Err(PushError::NameClash { first, second });
    }

    if let Some(path) = first_non_directory(&collection.trash_dir())? {
        return Err(PushError::TrashNotEmpty { path });
    }
    Ok(())
}

/// The first entry below `dir` that is not a directory, the directories
/// themselves being chains that hold nothing.
fn first_non_directory(dir: &Path) -> Result<Option<std::path::PathBuf>, PushError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(PushError::Trash {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| PushError::Trash {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|source| PushError::Trash {
            path: path.clone(),
            source,
        })?;
        if !kind.is_dir() {
            return Ok(Some(path));
        }
        if let Some(found) = first_non_directory(&path)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// The line for a version whose archive is staged because a repair could
/// not finish its move.
fn being_stored(piece: &RelPath, version: &StoreVersion, warnings: &[Warning]) -> String {
    let mut line = format!(
        "{piece} {}: its archive is still staged in `.antiq/tmp/pack/`",
        version.stamp
    );
    if !warnings.is_empty() {
        let texts: Vec<&str> = warnings.iter().map(|w| w.message.as_str()).collect();
        line.push_str(&format!(
            ". The repairs at the start of this run warned: {}",
            texts.join("; ")
        ));
    }
    line
}

/// The line that names a damaged or incomplete version and the command
/// that fits it, or `None` for a version that needs nothing.
pub(super) fn refusal(
    piece: &RelPath,
    version: &StoreVersion,
    condition: &Condition,
) -> Option<String> {
    let quoted = shell_quote(&piece.to_string());
    let stamp = version.stamp;
    let forget = format!("`antiq forget {quoted} {stamp}`");
    let copy_back = "or copy the version back from the target";
    let damages = match condition {
        Condition::Sound | Condition::NewerFormat | Condition::BeingStored => return None,
        Condition::Incomplete => {
            return Some(format!(
                "{piece} {stamp}: the archive is missing. Run {forget}, {copy_back}."
            ));
        }
        Condition::Damaged(damages) => damages,
    };

    let reasons: Vec<String> = damages.iter().map(ToString::to_string).collect();
    let head = format!("{piece} {stamp}: {}", reasons.join("; "));

    // `rebuild` writes a missing sidecar and nothing else. Any other damage
    // is for `forget`, which wins over `rebuild` when both would apply.
    let needs_forget = |damage: &Damage| !matches!(damage, Damage::NoListing | Damage::NoChecksum);
    if !damages.iter().any(needs_forget) {
        return Some(format!("{head}. Run `antiq rebuild {quoted} {stamp}`."));
    }

    // A sidecar that is damaged and not doubled can be deleted so that
    // `rebuild` writes it anew. The archive, which never changes, and a
    // doubled slot are beyond that.
    let archive_or_slot_damaged = damages
        .iter()
        .any(|damage| matches!(damage, Damage::Doubled { .. } | Damage::ArchiveSize { .. }));
    let mut deletable: Vec<String> = Vec::new();
    if !archive_or_slot_damaged {
        for damage in damages {
            let files = match damage {
                Damage::Head { .. } | Damage::Listing { .. } => &version.listings,
                Damage::Checksum { .. } => &version.checksums,
                _ => continue,
            };
            for file in files {
                let file = format!("`{}`", file.to_string_lossy());
                if !deletable.contains(&file) {
                    deletable.push(file);
                }
            }
        }
    }
    let mut line = format!("{head}. Run {forget}, {copy_back}");
    if !deletable.is_empty() {
        line.push_str(&format!(
            ", or delete {} and run `antiq rebuild {quoted} {stamp}`",
            deletable.join(" and ")
        ));
    }
    line.push('.');
    Some(line)
}

/// The two shortest paths that one name on the target's volume would
/// merge, looking at every folder on the way down to each path. The first
/// of the pair is the one that comes first in `paths`.
pub(super) fn find_clash(paths: &[RelPath]) -> Option<(RelPath, RelPath)> {
    let rules = TargetRules::macos();
    let deepest = paths
        .iter()
        .map(|path| path.components().count())
        .max()
        .unwrap_or(0);
    for depth in 1..=deepest {
        let mut seen: HashMap<Vec<u8>, RelPath> = HashMap::new();
        for path in paths {
            let components: Vec<&[u8]> = path.components().take(depth).collect();
            if components.len() < depth {
                continue;
            }
            let prefix = RelPath::from_bytes(components.join(&b'/'))
                .expect("a prefix of a valid path is a valid path");
            match seen.entry(fold_path(&prefix, &rules)) {
                Entry::Occupied(known) if *known.get() != prefix => {
                    return Some((known.get().clone(), prefix));
                }
                Entry::Occupied(_) => {}
                Entry::Vacant(slot) => {
                    slot.insert(prefix);
                }
            }
        }
    }
    None
}

/// `text` as one word of a POSIX shell.
fn shell_quote(text: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_./@%+=:,-".contains(c);
    if !text.is_empty() && text.chars().all(plain) {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::versions::damage::Damage;
    use crate::versions::names::VersionFile;
    use crate::versions::stamp::Stamp;
    use std::ffi::OsString;
    use std::fs;

    const STAMP: &str = "2024-06-30T081245";

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a path")
    }

    fn version(archives: &[&str], listings: &[&str], checksums: &[&str]) -> StoreVersion {
        let names = |names: &[&str]| names.iter().map(OsString::from).collect();
        StoreVersion {
            stamp: Stamp::parse(STAMP).expect("a stamp"),
            archives: names(archives),
            listings: names(listings),
            checksums: names(checksums),
        }
    }

    fn whole() -> StoreVersion {
        version(
            &["a--s.tar.zst"],
            &["a--s.sha256.zst"],
            &["a--s.tar.zst.sha256"],
        )
    }

    fn damaged(damage: Vec<Damage>) -> Condition {
        Condition::Damaged(damage)
    }

    fn line(piece: &str, version: &StoreVersion, condition: &Condition) -> String {
        refusal(&path(piece), version, condition).expect("a refusal")
    }

    #[test]
    fn a_missing_sidecar_alone_is_mended_by_rebuild() {
        for damage in [Damage::NoListing, Damage::NoChecksum] {
            let text = line("projects/a", &whole(), &damaged(vec![damage.clone()]));
            assert!(
                text.contains(&format!("`antiq rebuild projects/a {STAMP}`")),
                "{damage:?}: {text}"
            );
            assert!(!text.contains("antiq forget"), "{damage:?}: {text}");
        }
    }

    #[test]
    fn damage_that_rebuild_does_not_mend_is_forgotten_or_copied_back() {
        let doubled = Damage::Doubled {
            kind: VersionFile::Listing,
            files: vec!["x".into(), "y".into()],
        };
        let size = Damage::ArchiveSize {
            found: 1,
            recorded: 2,
        };
        for damage in [doubled, size] {
            let text = line("projects/a", &whole(), &damaged(vec![damage.clone()]));
            assert!(
                text.contains(&format!("`antiq forget projects/a {STAMP}`")),
                "{damage:?}: {text}"
            );
            assert!(
                text.contains("copy the version back from the target"),
                "{text}"
            );
            assert!(!text.contains("antiq rebuild"), "{damage:?}: {text}");
        }
    }

    #[test]
    fn a_damaged_sidecar_may_also_be_deleted_and_rebuilt() {
        let head = Damage::Head { reason: "r".into() };
        let listing = Damage::Listing { reason: "r".into() };
        let checksum = Damage::Checksum { reason: "r".into() };
        for (damage, file) in [
            (head, "a--s.sha256.zst"),
            (listing, "a--s.sha256.zst"),
            (checksum, "a--s.tar.zst.sha256"),
        ] {
            let text = line("projects/a", &whole(), &damaged(vec![damage.clone()]));
            assert!(
                text.contains(&format!("`antiq forget projects/a {STAMP}`")),
                "{damage:?}: {text}"
            );
            assert!(
                text.contains(&format!(
                    "delete `{file}` and run `antiq rebuild projects/a {STAMP}`"
                )),
                "{damage:?}: {text}"
            );
        }
    }

    #[test]
    fn forget_wins_when_a_version_has_several_damages() {
        let both = damaged(vec![
            Damage::NoListing,
            Damage::ArchiveSize {
                found: 1,
                recorded: 2,
            },
        ]);
        let text = line("projects/a", &whole(), &both);
        assert!(text.contains("antiq forget"), "{text}");
        assert!(!text.contains("antiq rebuild"), "{text}");
    }

    #[test]
    fn an_incomplete_version_is_forgotten_or_copied_back() {
        let sidecars = version(&[], &["a--s.sha256.zst"], &[]);
        let text = line("projects/a", &sidecars, &Condition::Incomplete);
        assert!(
            text.contains(&format!("`antiq forget projects/a {STAMP}`")),
            "{text}"
        );
        assert!(
            text.contains("copy the version back from the target"),
            "{text}"
        );
    }

    #[test]
    fn versions_that_are_sound_or_not_ours_to_judge_need_nothing() {
        for condition in [
            Condition::Sound,
            Condition::NewerFormat,
            Condition::BeingStored,
        ] {
            assert_eq!(refusal(&path("projects/a"), &whole(), &condition), None);
        }
    }

    #[test]
    fn the_piece_path_in_a_command_is_quoted_for_a_shell() {
        let text = line("my docs/it's", &whole(), &Condition::Incomplete);
        assert!(
            text.contains(&format!("`antiq forget 'my docs/it'\\''s' {STAMP}`")),
            "{text}"
        );
    }

    #[test]
    fn plain_words_stay_unquoted_and_the_rest_is_single_quoted() {
        assert_eq!(shell_quote("projects/a-b_c.d"), "projects/a-b_c.d");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    fn clash(paths: &[&str]) -> Option<(String, String)> {
        let paths: Vec<RelPath> = paths.iter().map(|text| path(text)).collect();
        find_clash(&paths).map(|(first, second)| (first.to_string(), second.to_string()))
    }

    #[test]
    fn two_paths_that_differ_in_case_clash() {
        assert_eq!(
            clash(&["Work", "work"]),
            Some(("Work".into(), "work".into()))
        );
    }

    #[test]
    fn two_paths_that_differ_in_normalisation_clash() {
        assert_eq!(
            clash(&["caf\u{e9}", "cafe\u{301}"]).map(|pair| pair.0.chars().count()),
            Some(4)
        );
    }

    #[test]
    fn a_clash_between_folders_above_the_paths_names_the_folders() {
        assert_eq!(
            clash(&["Work/a", "work/b"]),
            Some(("Work".into(), "work".into()))
        );
    }

    #[test]
    fn the_shortest_clashing_pair_is_named() {
        assert_eq!(
            clash(&["x/Deep/a", "x/deep/a", "Top", "top"]),
            Some(("Top".into(), "top".into()))
        );
    }

    #[test]
    fn paths_that_share_folders_or_differ_in_name_do_not_clash() {
        assert_eq!(clash(&["work/a", "work/b", "work/a", "other"]), None);
        assert_eq!(clash(&[]), None);
    }

    #[test]
    fn a_version_being_stored_names_the_warnings_of_the_repairs() {
        let piece = path("projects/a");
        let plain = being_stored(&piece, &whole(), &[]);
        assert!(
            plain.contains(STAMP) && plain.contains("tmp/pack"),
            "{plain}"
        );
        assert!(!plain.contains("warned"), "{plain}");

        let warned = being_stored(&piece, &whole(), &[Warning::new("cannot move x")]);
        assert!(warned.contains("warned: cannot move x"), "{warned}");
    }

    mod trash {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn a_missing_trash_and_a_chain_of_folders_hold_nothing() {
            let temp = tempfile::tempdir().expect("a temp dir");
            assert_eq!(
                first_non_directory(&temp.path().join("none")).expect("reads"),
                None
            );

            fs::create_dir_all(temp.path().join("a/b/c")).expect("made");
            assert_eq!(first_non_directory(temp.path()).expect("reads"), None);
        }

        #[test]
        fn a_file_or_symlink_deep_down_is_found() {
            let temp = tempfile::tempdir().expect("a temp dir");
            fs::create_dir_all(temp.path().join("a/b")).expect("made");
            std::os::unix::fs::symlink("nowhere", temp.path().join("a/b/link")).expect("link");

            let found = first_non_directory(temp.path()).expect("reads");

            assert_eq!(found, Some(temp.path().join("a/b/link")));
        }

        #[test]
        fn a_folder_that_cannot_be_read_is_an_error() {
            let temp = tempfile::tempdir().expect("a temp dir");
            let locked = temp.path().join("locked");
            fs::create_dir(&locked).expect("made");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("locked");
            assert!(
                fs::read_dir(&locked).is_err(),
                "the test runs as a user without access"
            );

            let result = first_non_directory(temp.path());

            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("unlocked");
            assert!(matches!(result, Err(PushError::Trash { .. })), "{result:?}");
        }
    }
}

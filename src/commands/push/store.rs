// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Step 2 of `push`: the store must be whole and the collection must not
//! hold names the target's volume would merge.

use super::PushError;
use crate::archive::TargetRules;
use crate::archive::names::fold_path;
use crate::code_span::CodeSpan;
use crate::collection::Collection;
use crate::filesystem::tree::TreeRoot;
use crate::relpath::{Name, RelPath};
use crate::repair::{Area, Repair};
use crate::report::Warning;
use crate::select::{Rules, select};
use crate::shown::Shown;
use crate::shown::{command_text, escape_controls};
use crate::versions::damage::{Check, Condition, Damage, Places, inspect_all};
use crate::versions::stamp::Stamp;
use crate::versions::store::{StoreVersion, lookup_exact, scan};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// What the repairs that a dry run only lists would have done by the time
/// the checks run.
#[derive(Debug, Default)]
pub(super) struct Planned {
    /// Versions of store folders that a repair deletes.
    versions: Vec<(RelPath, Stamp)>,
    /// Empty store folders that a repair removes.
    folders: Vec<RelPath>,
    /// Trees in `trash/` that a repair puts back or removes.
    trash: Vec<PathBuf>,
}

impl Planned {
    pub(super) fn from_repairs(collection: &Collection, repairs: &[Repair]) -> Planned {
        let mut planned = Planned::default();
        let trash = collection.trash_dir();
        let stamped = |stamp: &Option<Name>, tree: &RelPath| {
            let mut path = trash.clone();
            if let Some(stamp) = stamp {
                path.push(stamp.to_string());
            }
            path.join(tree.to_string())
        };
        // A repair names its piece in the live spelling, the store folder
        // may be spelled otherwise where the volume folds names. Nothing is
        // written by the lookup, so a dry run may ask. A failed lookup keeps
        // the live spelling, which can only make the check stricter.
        let pieces = TreeRoot::open(&collection.pieces_dir()).ok();
        let store_spelling = |piece: &RelPath| {
            pieces
                .as_ref()
                .and_then(|pieces| lookup_exact(pieces, piece).ok().flatten())
                .unwrap_or_else(|| piece.clone())
        };
        for repair in repairs {
            match repair {
                Repair::DeleteSidecars { piece, version } => {
                    planned.versions.push((store_spelling(piece), *version));
                }
                Repair::DeleteRestoredVersion { piece, version, .. } => {
                    planned.versions.push((store_spelling(piece), *version));
                }
                Repair::RemoveEmptyFolder { folder } => planned.folders.push(folder.clone()),
                Repair::GiveBack { stamp, tree, .. } => planned.trash.push(stamped(stamp, tree)),
                Repair::RemoveEmptyChain {
                    area: Area::Trash,
                    stamp,
                    path,
                } => planned.trash.push(stamped(&Some(stamp.clone()), path)),
                Repair::CreateFolder { .. }
                | Repair::DeleteTemporary { .. }
                | Repair::DeleteDoomed { .. }
                | Repair::RemoveEmptyChain { .. }
                | Repair::WriteReadme { .. } => {}
            }
        }
        planned
    }

    fn covers_version(&self, piece: &RelPath, version: &StoreVersion) -> bool {
        self.versions
            .iter()
            .any(|(path, stamp)| path == piece && *stamp == version.stamp)
    }

    fn covers_trash(&self, path: &Path) -> bool {
        self.trash.iter().any(|tree| path.starts_with(tree))
    }
}

/// When the checks run: after the repairs of a real run, or beside the
/// plan of a dry run.
pub(super) enum Mode<'a> {
    /// `warnings` are those of the repairs that ran just before.
    Real {
        warnings: &'a [Warning],
    },
    DryRun(Planned),
}

/// Refuses while the store holds a damaged or incomplete version, while
/// two live piece paths or two store folders would be one name on the
/// target's volume, and while `trash/` holds a tree. A dry run judges
/// the store as the listed repairs would leave it, and does not count a
/// staged archive as damage, since the repairs are what finish it.
/// Returns the live pieces and the store folders, which group the changes
/// of the mirror.
pub(super) fn check(collection: &Collection, mode: &Mode) -> Result<Vec<RelPath>, PushError> {
    let planned = match mode {
        Mode::Real { .. } => None,
        Mode::DryRun(planned) => Some(planned),
    };
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
        .filter(|(path, version)| !planned.is_some_and(|p| p.covers_version(path, version)))
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
    let mut commands = Vec::new();
    for ((piece, version), inspection) in items.iter().zip(inspections) {
        let inspection = inspection.map_err(PushError::Versions)?;
        if let Some(refusal) = verdict(piece, version, &inspection.condition, mode) {
            lines.push(escape_controls(&refusal.line));
            commands.extend(refusal.commands);
        }
    }
    if !lines.is_empty() {
        return Err(PushError::DamagedStore { lines, commands });
    }

    let store_folders: Vec<RelPath> = store
        .pieces
        .iter()
        .chain(&store.sidecar_only)
        .map(|piece| piece.path.clone())
        .filter(|path| !planned.is_some_and(|p| p.folders.contains(path)))
        .collect();
    if let Some((first, second)) = find_clash(&store_folders) {
        return Err(PushError::NameClash { first, second });
    }

    let config = &collection.config().collection;
    let root = open_root(collection.root())?;
    let rules = Rules::new(&config.patterns, &config.exclude)
        .and_then(|rules| rules.bind(&root))
        .map_err(PushError::Select)?;
    let selection = select(&root, &rules).map_err(PushError::Select)?;
    if let Some((first, second)) = find_clash(selection.pieces()) {
        return Err(PushError::NameClash { first, second });
    }

    let skip = |path: &Path| planned.is_some_and(|p| p.covers_trash(path));
    if let Some(path) = first_non_directory(&collection.trash_dir(), &skip)? {
        return Err(PushError::TrashNotEmpty { path });
    }
    let mut folders = store_folders;
    folders.extend(selection.pieces().iter().cloned());
    Ok(folders)
}

/// The collection root opened for listing.
fn open_root(path: &Path) -> Result<TreeRoot, PushError> {
    TreeRoot::open(path).map_err(|source| PushError::Root {
        path: path.to_path_buf(),
        source,
    })
}

/// The first entry below `dir` that is not a directory, the directories
/// themselves being chains that hold nothing.
fn first_non_directory(
    dir: &Path,
    skip: &dyn Fn(&Path) -> bool,
) -> Result<Option<PathBuf>, PushError> {
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
        if skip(&path) {
            continue;
        }
        let kind = entry.file_type().map_err(|source| PushError::Trash {
            path: path.clone(),
            source,
        })?;
        if !kind.is_dir() {
            return Ok(Some(path));
        }
        if let Some(found) = first_non_directory(&path, skip)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// A command of antiq that the line of a refused version tells to run.
///
/// The line writes it for a shell in the directory of the collection. A
/// console that knows how antiq was started can swap it for one that
/// works from anywhere, so the line and the swap come from the same data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionCommand {
    /// The command, such as `rebuild` or `forget`.
    pub verb: &'static str,
    pub piece: RelPath,
    pub stamp: Stamp,
}

impl VersionCommand {
    /// The words after `antiq`, with the piece as the bytes of its path.
    pub fn words(&self) -> Vec<OsString> {
        vec![
            OsString::from(self.verb),
            OsString::from_vec(self.piece.as_bytes().to_vec()),
            OsString::from(self.stamp.to_string()),
        ]
    }

    /// The command as the line of the version writes it: a code span, with
    /// the piece quoted for a shell.
    pub fn written(&self) -> String {
        CodeSpan(command_text(&self.words())).to_string()
    }
}

/// A line that refuses a version and the commands it tells to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Refusal {
    pub line: String,
    pub commands: Vec<VersionCommand>,
}

/// The refusal of a version in the condition `condition`, if any.
/// A staged archive is the repairs' business: a real run refuses it only
/// when a repair failed, and a dry run does not count it, since the
/// repairs it lists finish the move.
fn verdict(
    piece: &RelPath,
    version: &StoreVersion,
    condition: &Condition,
    mode: &Mode,
) -> Option<Refusal> {
    match (condition, mode) {
        (Condition::BeingStored, Mode::Real { warnings }) => Some(Refusal {
            line: being_stored(piece, version, warnings),
            commands: Vec::new(),
        }),
        (Condition::BeingStored, Mode::DryRun(_)) => None,
        _ => refusal(piece, version, condition),
    }
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
) -> Option<Refusal> {
    let stamp = version.stamp;
    let command = |verb| VersionCommand {
        verb,
        piece: piece.clone(),
        stamp,
    };
    let forget = command("forget");
    let rebuild = command("rebuild");
    let copy_back = "or copy the version back from the target";
    let damages = match condition {
        Condition::Sound | Condition::NewerFormat | Condition::BeingStored => return None,
        Condition::Incomplete => {
            return Some(Refusal {
                line: format!(
                    "{piece} {stamp}: the archive is missing. Run {}, {copy_back}.",
                    forget.written()
                ),
                commands: vec![forget],
            });
        }
        Condition::Damaged(damages) => damages,
    };

    let reasons: Vec<String> = damages.iter().map(ToString::to_string).collect();
    let head = format!("{piece} {stamp}: {}", reasons.join("; "));

    // `rebuild` writes a missing sidecar and nothing else. Any other damage
    // is for `forget`, which wins over `rebuild` when both would apply.
    let needs_forget = |damage: &Damage| !matches!(damage, Damage::NoListing | Damage::NoChecksum);
    if !damages.iter().any(needs_forget) {
        return Some(Refusal {
            line: format!("{head}. Run {}.", rebuild.written()),
            commands: vec![rebuild],
        });
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
                let file = CodeSpan(Shown(file)).to_string();
                if !deletable.contains(&file) {
                    deletable.push(file);
                }
            }
        }
    }
    let mut line = format!("{head}. Run {}, {copy_back}", forget.written());
    let mut commands = vec![forget];
    if !deletable.is_empty() {
        line.push_str(&format!(
            ", or delete {} and run {}",
            deletable.join(" and "),
            rebuild.written()
        ));
        commands.push(rebuild);
    }
    line.push('.');
    Some(Refusal { line, commands })
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
        refusal(&path(piece), version, condition)
            .expect("a refusal")
            .line
    }

    #[test]
    fn the_command_of_a_version_names_the_exact_piece() {
        let command = |piece: &[u8]| VersionCommand {
            verb: "rebuild",
            piece: RelPath::from_bytes(piece.to_vec()).expect("a path"),
            stamp: Stamp::parse(STAMP).expect("a stamp"),
        };
        assert_eq!(
            command(b"projects/100%").written(),
            format!("`antiq rebuild projects/100% {STAMP}`")
        );
        assert_eq!(
            command(b"projects/a\nb").written(),
            format!("`antiq rebuild $'projects/a\\nb' {STAMP}`")
        );
        let words = command(b"projects/100%").words();
        assert_eq!(
            words,
            [
                OsString::from("rebuild"),
                OsString::from("projects/100%"),
                OsString::from(STAMP)
            ]
        );
    }

    #[test]
    fn a_file_to_delete_is_shown_by_the_rule_for_names() {
        let mut stored = whole();
        stored.listings = vec![OsString::from("a%b\n--s.sha256.zst")];
        let text = line(
            "projects/a",
            &stored,
            &damaged(vec![Damage::Head { reason: "x".into() }]),
        );
        assert!(text.contains("delete `a%25b%0A--s.sha256.zst`"), "{text}");
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

    #[test]
    fn a_root_that_cannot_be_opened_is_named_in_the_error() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let missing = temp.path().join("gone");

        let result = open_root(&missing);

        match result {
            Err(PushError::Root { path, .. }) => assert_eq!(path, missing),
            other => panic!("expected a Root error, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn a_staged_version_is_refused_in_a_real_run_and_passed_in_a_dry_run() {
        let warnings = [Warning::new("cannot move x")];
        let real = Mode::Real {
            warnings: &warnings,
        };
        let line = verdict(
            &path("projects/a"),
            &whole(),
            &Condition::BeingStored,
            &real,
        )
        .expect("refused")
        .line;
        assert!(line.contains("cannot move x"), "{line}");

        let dry = Mode::DryRun(Planned::default());
        assert_eq!(
            verdict(&path("projects/a"), &whole(), &Condition::BeingStored, &dry),
            None
        );
        for mode in [&real, &dry] {
            assert!(verdict(&path("projects/a"), &whole(), &Condition::Incomplete, mode).is_some());
            assert_eq!(
                verdict(&path("projects/a"), &whole(), &Condition::Sound, mode),
                None
            );
        }
    }

    mod planned {
        use super::*;
        use crate::repair::Destination;

        fn name(text: &str) -> Name {
            Name::from_bytes(text.as_bytes().to_vec()).expect("a name")
        }

        fn stamp() -> Stamp {
            Stamp::parse(STAMP).expect("a stamp")
        }

        /// A collection in a temp dir whose store holds the folder `stored`,
        /// and whether that volume treats `LOWER` and `lower` as one name.
        fn collection_with_store_folder(
            temp: &tempfile::TempDir,
            stored: &str,
        ) -> (Collection, bool) {
            let root = fs::canonicalize(temp.path()).expect("the temp dir exists");
            fs::write(root.join("antiq-probe-case"), "").expect("written");
            let folds = root.join("ANTIQ-PROBE-CASE").exists();
            fs::create_dir_all(root.join(".antiq/pieces").join(stored)).expect("made");
            let id = crate::testing::tree::COLLECTION_ID
                .parse()
                .expect("the fixture ID is a ULID");
            (Collection::new(root, crate::config::Config::new(id)), folds)
        }

        fn delete_restored(piece: &str) -> Repair {
            Repair::DeleteRestoredVersion {
                piece: path(piece),
                version: stamp(),
                delete: true,
            }
        }

        #[test]
        fn a_repair_in_the_live_spelling_covers_the_store_folder_where_the_volume_folds() {
            let temp = tempfile::tempdir().expect("a temp dir");
            let (collection, folds) = collection_with_store_folder(&temp, "Projects/A");
            if !folds {
                eprintln!("skipped: the volume of the temp dir is case-sensitive");
                return;
            }

            let planned = Planned::from_repairs(&collection, &[delete_restored("projects/a")]);

            assert!(planned.covers_version(&path("Projects/A"), &whole_version()));
        }

        #[test]
        fn a_repair_in_another_case_covers_nothing_where_the_volume_keeps_case() {
            let temp = tempfile::tempdir().expect("a temp dir");
            let (collection, folds) = collection_with_store_folder(&temp, "Projects/A");
            if folds {
                eprintln!("skipped: the volume of the temp dir folds case");
                return;
            }

            let planned = Planned::from_repairs(&collection, &[delete_restored("projects/a")]);

            assert!(!planned.covers_version(&path("Projects/A"), &whole_version()));
            assert!(planned.covers_version(&path("projects/a"), &whole_version()));
        }

        fn whole_version() -> StoreVersion {
            version(&["a--s.tar.zst"], &[], &[])
        }

        #[test]
        fn repairs_that_remove_versions_folders_and_trash_trees_are_collected() {
            let collection = Collection::for_tests();
            let repairs = [
                Repair::CreateFolder {
                    folder: name("tmp"),
                },
                Repair::DeleteSidecars {
                    piece: path("projects/a"),
                    version: stamp(),
                },
                Repair::DeleteRestoredVersion {
                    piece: path("projects/b"),
                    version: stamp(),
                    delete: true,
                },
                Repair::RemoveEmptyFolder {
                    folder: path("empty"),
                },
                Repair::GiveBack {
                    stamp: Some(name("2024")),
                    tree: path("t/one"),
                    destination: Destination::Live,
                },
                Repair::GiveBack {
                    stamp: None,
                    tree: path("loose"),
                    destination: Destination::Recovered,
                },
                Repair::RemoveEmptyChain {
                    area: Area::Trash,
                    stamp: name("2025"),
                    path: path("chain"),
                },
                Repair::RemoveEmptyChain {
                    area: Area::Doomed,
                    stamp: name("2026"),
                    path: path("other"),
                },
                Repair::DeleteTemporary { entry: name("x") },
            ];

            let planned = Planned::from_repairs(&collection, &repairs);

            let whole = version(&["a--s.tar.zst"], &[], &[]);
            assert!(planned.covers_version(&path("projects/a"), &whole));
            assert!(planned.covers_version(&path("projects/b"), &whole));
            assert!(!planned.covers_version(&path("projects/c"), &whole));
            assert_eq!(planned.folders, [path("empty")]);
            let trash = collection.trash_dir();
            assert!(planned.covers_trash(&trash.join("2024/t/one/file.txt")));
            assert!(planned.covers_trash(&trash.join("loose")));
            assert!(planned.covers_trash(&trash.join("2025/chain")));
            assert!(!planned.covers_trash(&trash.join("2024/t/two")));
            assert!(!planned.covers_trash(&trash.join("2026/other")));
        }
    }

    mod trash {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn a_missing_trash_and_a_chain_of_folders_hold_nothing() {
            let temp = tempfile::tempdir().expect("a temp dir");
            assert_eq!(
                first_non_directory(&temp.path().join("none"), &|_| false).expect("reads"),
                None
            );

            fs::create_dir_all(temp.path().join("a/b/c")).expect("made");
            assert_eq!(
                first_non_directory(temp.path(), &|_| false).expect("reads"),
                None
            );
        }

        #[test]
        fn a_file_or_symlink_deep_down_is_found() {
            let temp = tempfile::tempdir().expect("a temp dir");
            fs::create_dir_all(temp.path().join("a/b")).expect("made");
            std::os::unix::fs::symlink("nowhere", temp.path().join("a/b/link")).expect("link");

            let found = first_non_directory(temp.path(), &|_| false).expect("reads");

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

            let result = first_non_directory(temp.path(), &|_| false);

            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("unlocked");
            assert!(matches!(result, Err(PushError::Trash { .. })), "{result:?}");
        }
    }
}

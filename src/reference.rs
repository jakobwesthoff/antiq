// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Piece references.

use crate::code_span::CodeSpan;
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::filesystem::tree::TreeRoot;
use crate::query::Query;
use crate::relpath::{RelPath, RelPathError, below_root, escape};
use crate::select::Selection;
use crate::versions::VersionsError;
use crate::versions::store::{StoreScan, lookup_exact};
use serde::Serialize;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// A piece a reference could mean, by its path in the escaped form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub path: String,
}

/// The first of the pieces an ambiguous reference matches, as the example
/// of a longer reference. An ambiguous reference always has candidates.
fn example_path(candidates: &[Candidate]) -> CodeSpan<&str> {
    CodeSpan(candidates.first().map_or("", |first| first.path.as_str()))
}

/// Errors of resolving a piece reference.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ReferenceError {
    #[diagnostic(
        code(antiq::reference::ambiguous),
        help(
            "Name the piece by more of its path, for example {}.",
            example_path(.candidates)
        )
    )]
    #[error(
        "no unique piece matches {}, as it fits the paths of {} pieces",
        CodeSpan(.reference),
        candidates.len()
    )]
    Ambiguous {
        reference: String,
        candidates: Vec<Candidate>,
    },
    #[diagnostic(
        code(antiq::reference::inexact),
        help(
            "Run the command again with the path of the piece you mean, for example {}.",
            CodeSpan(.example)
        )
    )]
    #[error(
        "{} is not the path of a piece, and this command acts only on a path",
        CodeSpan(.reference)
    )]
    Inexact {
        reference: String,
        candidates: Vec<Candidate>,
        /// The first candidate spelled as a path that names it from the
        /// directory the command runs in: relative to the collection root
        /// at the root, absolute elsewhere. It is the raw text of the path
        /// when that is UTF-8, so a `%` in a name is not read as an escape.
        example: String,
    },
    #[diagnostic(
        code(antiq::reference::unknown),
        help("Check the spelling, or name the piece by its path.")
    )]
    #[error(
        "no piece matches {}, as none of the pieces has a path that fits it",
        CodeSpan(.reference)
    )]
    Unknown {
        reference: String,
        /// The pieces nearest to the reference by spelling, nearest first.
        /// The console offers them; they are not part of the JSON error.
        closest: Vec<Candidate>,
    },
    #[diagnostic(
        code(antiq::reference::only_incomplete),
        help("`antiq forget` removes what is left of them.")
    )]
    #[error(
        "{} matches no piece with an archive, as {} holds only incomplete versions",
        CodeSpan(.reference),
        CodeSpan(.piece)
    )]
    OnlyIncomplete { reference: String, piece: String },
    #[diagnostic(
        code(antiq::reference::outside),
        help("Name a path below {}.", CodeSpan(.root))
    )]
    #[error(
        "{} is outside the antiq collection at {}",
        CodeSpan(.path),
        CodeSpan(.root)
    )]
    Outside { path: String, root: String },
    #[error("cannot look at the collection root {path}: {source}")]
    Root { path: String, source: io::Error },
    #[error("cannot open {path} to look for `{reference}`: {source}")]
    Open {
        path: String,
        reference: String,
        source: io::Error,
    },
    #[error("cannot look for `{reference}`: {source}")]
    Lookup {
        reference: String,
        source: Box<VersionsError>,
    },
    #[error(transparent)]
    #[diagnostic(transparent)]
    Path(#[from] RelPathError),
}

impl HasKind for ReferenceError {
    fn kind(&self) -> ErrorKind {
        match self {
            ReferenceError::Ambiguous { .. } => ErrorKind::AmbiguousReference,
            ReferenceError::Inexact { .. } => ErrorKind::InexactReference,
            ReferenceError::Unknown { .. } | ReferenceError::OnlyIncomplete { .. } => {
                ErrorKind::UnknownReference
            }
            ReferenceError::Outside { .. } => ErrorKind::OutsideCollection,
            ReferenceError::Root { .. }
            | ReferenceError::Open { .. }
            | ReferenceError::Lookup { .. }
            | ReferenceError::Path(_) => ErrorKind::Other,
        }
    }
}

/// The path `given` leads to, relative to the collection root.
///
/// `.` and `..` are folded from the text alone. Then the shortest leading
/// part that is the collection root itself, by device and inode, stands
/// for the root, so another spelling of the root (`/var` for
/// `/private/var` on macOS, or a different case on a case-insensitive
/// volume) still lies below it. What follows is taken literally: nothing
/// below the root is looked at on disk, so a symlink there is never
/// followed. A path with no such part is outside the collection.
///
/// The root itself and paths below `.antiq/` are not refused here; callers
/// that cannot take them refuse them.
///
/// `cwd` is the directory relative paths start at: the canonical form of
/// the `-C` directory, else the process's current directory. That is the
/// directory `Collection::find` started from.
pub fn resolve_path(
    collection: &Collection,
    cwd: &Path,
    given: &Path,
) -> Result<RelPath, ReferenceError> {
    let root = collection.root();
    let root_identity = identity(root).map_err(|source| ReferenceError::Root {
        path: escape(root.as_os_str().as_bytes()),
        source,
    })?;

    let folded = match below_root(Path::new("/"), given, cwd) {
        Ok(folded) => folded,
        Err(RelPathError::AboveRoot(_)) => {
            return Err(ReferenceError::Outside {
                path: escape(given.as_os_str().as_bytes()),
                root: escape(root.as_os_str().as_bytes()),
            });
        }
        Err(error) => return Err(error.into()),
    };

    let names: Vec<&[u8]> = folded.components().collect();
    let mut prefix = PathBuf::from("/");
    for taken in 0..=names.len() {
        if taken > 0 {
            prefix.push(OsStr::from_bytes(names[taken - 1]));
        }
        if identity(&prefix).ok() == Some(root_identity) {
            return Ok(RelPath::from_bytes(names[taken..].join(&b'/'))?);
        }
    }
    Err(ReferenceError::Outside {
        path: escape(
            Path::new("/")
                .join(OsStr::from_bytes(folded.as_bytes()))
                .as_os_str()
                .as_bytes(),
        ),
        root: escape(root.as_os_str().as_bytes()),
    })
}

// =========================================================
// Candidate sets
// =========================================================

/// The pieces a command can mean by a reference, and the directory in which
/// the volume decides whether a differently spelled path names one of them.
#[derive(Debug, Clone)]
pub struct CandidateSet {
    /// Sorted by path.
    paths: Vec<RelPath>,
    directory: PathBuf,
}

impl CandidateSet {
    fn new(mut paths: Vec<RelPath>, directory: PathBuf) -> CandidateSet {
        paths.sort();
        CandidateSet { paths, directory }
    }

    /// The live pieces, for `archive`, `pin` and `unpin`.
    pub fn live(collection: &Collection, selection: &Selection) -> CandidateSet {
        CandidateSet::new(selection.pieces().to_vec(), collection.root().to_path_buf())
    }

    /// The live pieces and the paths `extra`, which are not live yet but
    /// will be: a dry run adds the folders that a repair would give back.
    pub fn live_with(
        collection: &Collection,
        selection: &Selection,
        extra: &[RelPath],
    ) -> CandidateSet {
        let mut paths = selection.pieces().to_vec();
        paths.extend(extra.iter().cloned());
        paths.sort();
        paths.dedup();
        CandidateSet::new(paths, collection.root().to_path_buf())
    }

    /// The pieces of the store, for `unearth`, `fork` and `rebuild`.
    pub fn store(collection: &Collection, scan: &StoreScan) -> CandidateSet {
        let paths = scan.pieces.iter().map(|piece| piece.path.clone());
        CandidateSet::new(paths.collect(), collection.pieces_dir())
    }

    /// The pieces of the store and the store folders that hold sidecars
    /// only, for `forget`.
    pub fn store_and_sidecar_only(collection: &Collection, scan: &StoreScan) -> CandidateSet {
        let paths = scan
            .pieces
            .iter()
            .chain(&scan.sidecar_only)
            .map(|piece| piece.path.clone());
        CandidateSet::new(paths.collect(), collection.pieces_dir())
    }
}

impl CandidateSet {
    /// The candidate `path` names, in the spelling the candidate has.
    ///
    /// A path equal to a candidate byte for byte is that candidate. On a
    /// volume that treats other spellings as the same name, the directory
    /// the set lives in tells the spelling it is stored under. That
    /// directory is not there when the store has no `pieces/` yet, and then
    /// no other spelling can name a candidate.
    fn find_path(
        &self,
        path: &RelPath,
        reference: &str,
    ) -> Result<Option<&RelPath>, ReferenceError> {
        if let Some(candidate) = self.paths.iter().find(|candidate| *candidate == path) {
            return Ok(Some(candidate));
        }
        let directory = match TreeRoot::open(&self.directory) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(ReferenceError::Open {
                    path: escape(self.directory.as_os_str().as_bytes()),
                    reference: reference.to_string(),
                    source,
                });
            }
        };
        let spelled = lookup_exact(&directory, path).map_err(|source| ReferenceError::Lookup {
            reference: reference.to_string(),
            source: Box::new(source),
        })?;
        Ok(spelled.and_then(|spelled| self.paths.iter().find(|candidate| **candidate == spelled)))
    }
}

/// What a command does with a search word that fits exactly one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchWord {
    /// The command acts on that candidate.
    Acts,
    /// The command removes something, so it acts only on the exact path of
    /// a candidate and offers the one match in an error instead.
    OnlyOffers,
}

/// The piece of `set` that the reference `given` means.
///
/// An input that names a candidate as a path is that piece. Any other
/// input is a search word, matched against the escaped paths of the
/// candidates, and must match exactly one. What happens with that one
/// match is up to `word`.
pub fn resolve_piece(
    collection: &Collection,
    cwd: &Path,
    given: &OsStr,
    set: &CandidateSet,
    word: SearchWord,
) -> Result<RelPath, ReferenceError> {
    let reference = escape(given.as_bytes());
    let path = resolve_path(collection, cwd, Path::new(given))?;
    if let Some(candidate) = set.find_path(&path, &reference)? {
        return Ok(candidate.clone());
    }

    let mut query = Query::parse(&reference);
    let mut matches: Vec<&RelPath> = set
        .paths
        .iter()
        .filter(|candidate| query.score(&candidate.to_string()).is_some())
        .collect();
    match matches.len() {
        0 => Err(ReferenceError::Unknown {
            closest: closest_pieces(&reference, &set.paths),
            reference,
        }),
        1 if word == SearchWord::OnlyOffers => {
            let offered = matches.remove(0);
            Err(ReferenceError::Inexact {
                example: exact_spelling(collection, cwd, offered),
                candidates: vec![Candidate {
                    path: offered.to_string(),
                }],
                reference,
            })
        }
        1 => Ok(matches.remove(0).clone()),
        _ => Err(ReferenceError::Ambiguous {
            reference,
            candidates: matches
                .into_iter()
                .map(|candidate| Candidate {
                    path: candidate.to_string(),
                })
                .collect(),
        }),
    }
}

/// `candidate` spelled so that it is an exact path from `cwd`, the
/// directory the command runs in.
///
/// A path is read against that directory, so the root-relative path the
/// output shows names the candidate only at the collection root. Anywhere
/// else the absolute path does. The text is raw where the bytes are UTF-8:
/// the escaped form of a name that holds a `%` is the name of another path.
fn exact_spelling(collection: &Collection, cwd: &Path, candidate: &RelPath) -> String {
    let at_root = matches!(
        resolve_path(collection, cwd, Path::new(".")),
        Ok(here) if here == RelPath::root()
    );
    let spelled = if at_root {
        PathBuf::from(OsStr::from_bytes(candidate.as_bytes()))
    } else {
        collection
            .root()
            .join(OsStr::from_bytes(candidate.as_bytes()))
    };
    match spelled.into_os_string().into_string() {
        Ok(text) => text,
        Err(_) if at_root => candidate.to_string(),
        Err(_) => escape(
            collection
                .root()
                .join(OsStr::from_bytes(candidate.as_bytes()))
                .as_os_str()
                .as_bytes(),
        ),
    }
}

/// How many pieces an unknown reference offers as the ones it may have
/// meant.
const CLOSEST_LIMIT: usize = 3;

/// The pieces of `paths` whose spelling is within a few edits of `reference`,
/// nearest first and by path among equals.
///
/// A fuzzy match needs the letters in order, so a typo that swaps two
/// letters finds nothing there; the edit distance catches it. The reference
/// is compared with the whole path and with its last name, because people
/// type either. A few edits is two, or a third of the reference when that
/// is more, so short words stay strict and long paths forgive more.
fn closest_pieces(reference: &str, paths: &[RelPath]) -> Vec<Candidate> {
    let reference = reference.to_lowercase();
    let allowed = (reference.chars().count() / 3).max(2);
    let mut near: Vec<(usize, String)> = paths
        .iter()
        .filter_map(|path| {
            let whole = path.to_string();
            let lowered = whole.to_lowercase();
            let last = lowered.rsplit('/').next().unwrap_or(&lowered);
            let distance = strsim::osa_distance(&reference, &lowered)
                .min(strsim::osa_distance(&reference, last));
            (distance <= allowed).then_some((distance, whole))
        })
        .collect();
    near.sort();
    near.into_iter()
        .take(CLOSEST_LIMIT)
        .map(|(_, path)| Candidate { path })
        .collect()
}

/// Resolves a piece of the store for a command that works on archives
/// (`unearth`, `fork`, `rebuild`). A folder that holds only sidecars is no
/// piece for them, but whoever names it is told what to do with it.
pub fn resolve_stored_piece(
    collection: &Collection,
    cwd: &Path,
    given: &OsStr,
    scan: &StoreScan,
    word: SearchWord,
) -> Result<RelPath, ReferenceError> {
    match resolve_piece(
        collection,
        cwd,
        given,
        &CandidateSet::store(collection, scan),
        word,
    ) {
        Err(ReferenceError::Unknown { reference, closest }) => {
            // Whatever this finds only words the error, it never acts, so a
            // search word may find its one folder here.
            let wider = CandidateSet::store_and_sidecar_only(collection, scan);
            Err(
                match resolve_piece(collection, cwd, given, &wider, SearchWord::Acts) {
                    Ok(piece) => ReferenceError::OnlyIncomplete {
                        reference,
                        piece: piece.to_string(),
                    },
                    Err(_) => ReferenceError::Unknown { reference, closest },
                },
            )
        }
        other => other,
    }
}

/// `path` with the longest prefix that exists below `root` in the spelling
/// it is stored under.
///
/// A new store folder below an existing parent goes into the parent's
/// stored spelling on a volume that treats other spellings as the same name.
pub fn stored_spelling(root: &TreeRoot, path: &RelPath) -> Result<RelPath, ReferenceError> {
    let names: Vec<&[u8]> = path.components().collect();
    for taken in (1..=names.len()).rev() {
        let prefix = RelPath::from_bytes(names[..taken].join(&b'/'))
            .expect("leading names of a valid path are a valid path");
        let found = lookup_exact(root, &prefix).map_err(|source| ReferenceError::Lookup {
            reference: path.to_string(),
            source: Box::new(source),
        })?;
        if let Some(stored) = found {
            let rest = RelPath::from_bytes(names[taken..].join(&b'/'))
                .expect("trailing names of a valid path are a valid path");
            return Ok(stored.concat(&rest));
        }
    }
    Ok(path.clone())
}

/// Device and inode of the directory `path` leads to, following symlinks
/// in `path` itself.
fn identity(path: &Path) -> io::Result<(u64, u64)> {
    let entry = TreeRoot::open(path)?.lstat(&RelPath::root())?;
    Ok((entry.device, entry.inode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::TempCollection;
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a path")
    }

    struct Fixture {
        temp: TempCollection,
        collection: Collection,
        /// A directory next to the collection, for paths outside it.
        outside: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Fixture {
            let temp = TempCollection::minimal();
            let collection = Collection::find(temp.root()).expect("a collection");
            Fixture {
                temp,
                collection,
                outside: tempfile::tempdir().expect("a temp directory"),
            }
        }

        fn root(&self) -> PathBuf {
            self.collection.root().to_path_buf()
        }

        fn resolve_path(&self, given: impl AsRef<Path>) -> Result<RelPath, ReferenceError> {
            resolve_path(&self.collection, &self.root(), given.as_ref())
        }
    }

    fn kind(error: &ReferenceError) -> ErrorKind {
        error.kind()
    }

    fn outside_parts(error: ReferenceError) -> Option<(String, String)> {
        match error {
            ReferenceError::Outside { path, root } => Some((path, root)),
            _ => None,
        }
    }

    #[test]
    fn a_path_below_the_root_resolves_without_its_folder() {
        let fx = Fixture::new();
        assert!(!fx.root().join("projects").exists(), "no folder on disk");
        let given = fx.root().join("projects/alpha");
        assert_eq!(
            fx.resolve_path(given).expect("resolves"),
            path("projects/alpha")
        );
    }

    #[test]
    fn a_relative_path_starts_at_the_current_directory() {
        let fx = Fixture::new();
        let cwd = fx.root().join("projects");
        let from = |given: &str| resolve_path(&fx.collection, &cwd, Path::new(given));
        assert_eq!(from("alpha").expect("resolves"), path("projects/alpha"));
        assert_eq!(
            from("../projects/./alpha").expect("resolves"),
            path("projects/alpha")
        );
    }

    #[test]
    fn dot_and_dotdot_are_folded_from_the_text() {
        let fx = Fixture::new();
        assert_eq!(
            fx.resolve_path("./a/../proj").expect("resolves"),
            path("proj")
        );
        assert_eq!(
            fx.resolve_path("a/b/../../proj/").expect("resolves"),
            path("proj")
        );
    }

    #[test]
    fn an_absolute_path_through_a_symlink_to_the_root_resolves() {
        let fx = Fixture::new();
        let link = fx.outside.path().join("rootlink");
        symlink(fx.root(), &link).expect("symlink");
        assert_eq!(
            fx.resolve_path(link.join("projects/alpha"))
                .expect("resolves"),
            path("projects/alpha")
        );
        assert_eq!(fx.resolve_path(&link).expect("the root"), RelPath::root());
    }

    #[test]
    fn a_symlink_below_the_root_is_not_followed() {
        let mut fx = Fixture::new();
        fx.temp.tree().dir("other");
        // `link` leads to the folder `other`; `back` leads to the root.
        symlink(fx.root().join("other"), fx.root().join("link")).expect("symlink");
        symlink(fx.root(), fx.root().join("back")).expect("symlink");
        assert_eq!(fx.resolve_path("link").expect("inside"), path("link"));
        assert_eq!(
            fx.resolve_path("back/other").expect("inside"),
            path("back/other")
        );
    }

    #[test]
    fn dotdot_after_a_symlink_folds_lexically() {
        let fx = Fixture::new();
        // Physically `dirlink/..` is the outside directory's parent.
        let elsewhere = fx.outside.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("dir");
        symlink(&elsewhere, fx.root().join("dirlink")).expect("symlink");
        assert_eq!(
            fx.resolve_path("dirlink/../projects/alpha")
                .expect("resolves"),
            path("projects/alpha")
        );
    }

    #[test]
    fn a_path_outside_the_collection_is_refused() {
        let fx = Fixture::new();
        let given = fx.outside.path().join("proj");
        let error = fx.resolve_path(&given).expect_err("outside");
        assert_eq!(kind(&error), ErrorKind::OutsideCollection);
        let (path, root) = outside_parts(error).expect("is outside");
        assert_eq!(path, given.to_string_lossy());
        assert_eq!(root, fx.root().to_string_lossy());
    }

    #[test]
    fn dotdot_out_of_the_root_is_refused() {
        let fx = Fixture::new();
        let error = fx.resolve_path("../sibling").expect_err("outside");
        assert_eq!(kind(&error), ErrorKind::OutsideCollection);
    }

    #[test]
    fn dotdot_above_the_filesystem_root_is_refused_as_outside() {
        let fx = Fixture::new();
        let error = fx.resolve_path("/../../x").expect_err("above the root");
        assert_eq!(kind(&error), ErrorKind::OutsideCollection);
        assert!(matches!(
            fx.resolve_path("/..").expect_err("above the root"),
            ReferenceError::Outside { .. }
        ));
    }

    #[test]
    fn a_differently_cased_spelling_of_the_root_resolves() {
        let fx = Fixture::new();
        let swapped: String = fx
            .root()
            .to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        if std::fs::metadata(&swapped).is_err() {
            eprintln!("skipped: the temp volume is case sensitive");
            return;
        }
        let given = PathBuf::from(swapped).join("projects/alpha");
        assert_eq!(
            fx.resolve_path(given).expect("resolves"),
            path("projects/alpha")
        );
    }

    #[test]
    fn a_folder_without_an_object_resolves_to_its_path() {
        let mut fx = Fixture::new();
        fx.temp.tree().dir("new-piece");
        assert_eq!(
            fx.resolve_path("new-piece").expect("a path"),
            path("new-piece")
        );
    }

    #[test]
    fn the_collection_root_resolves_to_the_empty_path() {
        let fx = Fixture::new();
        assert_eq!(fx.resolve_path(".").expect("the root"), RelPath::root());
    }

    #[test]
    fn a_name_that_is_not_utf8_resolves_by_its_bytes() {
        let fx = Fixture::new();
        let mut given = fx.root().into_os_string().into_vec();
        given.extend_from_slice(b"/d\xff");
        let given = OsStr::from_bytes(&given);
        assert_eq!(fx.resolve_path(given).expect("resolves"), path("d%FF"));
        let error = fx
            .resolve_path(OsStr::from_bytes(b"/elsewhere/d\xff"))
            .expect_err("outside");
        let (path, _) = outside_parts(error).expect("is outside");
        assert_eq!(path, "/elsewhere/d%FF", "the escaped form names the input");
    }

    #[test]
    fn a_collection_root_that_cannot_be_looked_at_is_an_error() {
        let fx = Fixture::new();
        let gone = Collection::for_tests();
        let error = resolve_path(&gone, &fx.root(), Path::new("x")).expect_err("no root");
        assert!(matches!(error, ReferenceError::Root { .. }), "{error:?}");
        assert_eq!(kind(&error), ErrorKind::Other);
        assert!(
            error.to_string().contains("collection-for-tests"),
            "{error}"
        );
    }

    #[test]
    fn a_current_directory_that_is_not_absolute_is_a_path_error() {
        let fx = Fixture::new();
        let error = resolve_path(&fx.collection, Path::new("relative"), Path::new("x"))
            .expect_err("not absolute");
        assert!(matches!(error, ReferenceError::Path(_)), "{error:?}");
        assert_eq!(kind(&error), ErrorKind::Other);
    }

    // =====================================================
    // Piece references
    // =====================================================

    use crate::filesystem::RestoreModes;
    use crate::select::{Rules, select};
    use crate::versions::names::{VersionFile, file_name};
    use crate::versions::stamp::Stamp;
    use crate::versions::store::scan;
    use std::os::unix::fs::PermissionsExt;

    /// A set over `paths`, with the collection's `pieces/` as the place the
    /// volume is asked about spelling.
    fn set_of(fx: &Fixture, paths: &[&str]) -> CandidateSet {
        CandidateSet::new(
            paths.iter().map(|text| path(text)).collect(),
            fx.collection.pieces_dir(),
        )
    }

    fn resolve_in(
        fx: &Fixture,
        set: &CandidateSet,
        given: impl AsRef<OsStr>,
    ) -> Result<RelPath, ReferenceError> {
        resolve_piece(
            &fx.collection,
            &fx.root(),
            given.as_ref(),
            set,
            SearchWord::Acts,
        )
    }

    /// Whether the volume of the temp directory finds `LOWER` for `lower`.
    fn folds_case(fx: &mut Fixture) -> bool {
        fx.temp.tree().file("antiq-probe-case", b"");
        fx.temp.tree().path("ANTIQ-PROBE-CASE").exists()
    }

    /// The three files of a version of `piece` in its store folder.
    fn store_version(fx: &mut Fixture, piece: &str, kinds: &[VersionFile]) {
        let stamp = Stamp::parse("2026-11-04T090211").expect("a stamp");
        for kind in kinds {
            let name = file_name(&path(piece), &stamp, *kind);
            fx.temp
                .tree()
                .file(&format!(".antiq/pieces/{piece}/{name}"), b"x");
        }
    }

    fn live_selection(fx: &Fixture, patterns: &[&str]) -> Selection {
        let root = TreeRoot::open(fx.collection.root()).expect("the root opens");
        let patterns: Vec<String> = patterns.iter().map(|text| text.to_string()).collect();
        let rules = Rules::new(&patterns, &[])
            .expect("valid patterns")
            .bind(&root)
            .expect("binds");
        select(&root, &rules).expect("selects")
    }

    fn texts(set: &CandidateSet) -> Vec<String> {
        set.paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn the_live_set_holds_the_selected_pieces_in_path_order() {
        let mut fx = Fixture::new();
        fx.temp.tree().dir("projects/beta");
        fx.temp.tree().dir("projects/alpha");
        let selection = live_selection(&fx, &["projects/*"]);
        let set = CandidateSet::live(&fx.collection, &selection);
        assert_eq!(texts(&set), ["projects/alpha", "projects/beta"]);
    }

    #[test]
    fn extra_paths_join_the_live_set_once_and_in_path_order() {
        let mut fx = Fixture::new();
        fx.temp.tree().dir("projects/beta");
        let selection = live_selection(&fx, &["projects/*"]);
        let extra = [path("projects/gamma"), path("projects/beta")];
        let set = CandidateSet::live_with(&fx.collection, &selection, &extra);
        assert_eq!(texts(&set), ["projects/beta", "projects/gamma"]);
    }

    #[test]
    fn the_store_set_holds_pieces_and_the_forget_set_adds_sidecar_only_folders() {
        let mut fx = Fixture::new();
        store_version(&mut fx, "projects/alpha", &VersionFile::ALL);
        store_version(
            &mut fx,
            "projects/gone",
            &[VersionFile::Listing, VersionFile::Checksum],
        );
        let found = scan(&fx.collection.pieces_dir()).expect("scans");
        assert_eq!(
            texts(&CandidateSet::store(&fx.collection, &found)),
            ["projects/alpha"]
        );
        assert_eq!(
            texts(&CandidateSet::store_and_sidecar_only(
                &fx.collection,
                &found
            )),
            ["projects/alpha", "projects/gone"]
        );
    }

    #[test]
    fn a_path_selects_its_piece_through_resolve_piece() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/alpha", "projects/beta"]);
        let given = fx.root().join("projects/beta");
        assert_eq!(
            resolve_in(&fx, &set, given).expect("resolves"),
            path("projects/beta")
        );
    }

    #[test]
    fn a_relative_path_starts_at_the_current_directory_through_resolve_piece() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/alpha", "projects/beta"]);
        let cwd = fx.root().join("projects");
        let found = resolve_piece(
            &fx.collection,
            &cwd,
            OsStr::new("beta"),
            &set,
            SearchWord::Acts,
        );
        assert_eq!(found.expect("resolves"), path("projects/beta"));
    }

    #[test]
    fn a_path_beats_a_word_that_would_match_two_pieces() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/alpha", "projects/alphabet"]);
        assert_eq!(
            resolve_in(&fx, &set, "projects/alpha").expect("the path wins"),
            path("projects/alpha")
        );
        let error = resolve_in(&fx, &set, "alph").expect_err("the word is ambiguous");
        assert!(
            matches!(error, ReferenceError::Ambiguous { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_path_naming_a_folder_that_is_no_candidate_is_a_word() {
        let mut fx = Fixture::new();
        fx.temp.tree().dir(".antiq/pieces/projects/alpha");
        let set = set_of(&fx, &["projects/alpha", "other/thing"]);
        // `projects` exists in the store but is not a piece, so it is a word
        // that matches the one piece below it.
        assert_eq!(
            resolve_in(&fx, &set, "projects").expect("resolves by search"),
            path("projects/alpha")
        );
    }

    #[test]
    fn a_path_with_another_case_finds_the_stored_spelling_on_a_volume_that_folds() {
        let mut fx = Fixture::new();
        let folds = folds_case(&mut fx);
        fx.temp.tree().dir(".antiq/pieces/projects/alpha");
        let set = set_of(&fx, &["projects/alpha", "projects/beta"]);
        let found = resolve_in(&fx, &set, fx.root().join("Projects/ALPHA"));
        if folds {
            assert_eq!(
                found.expect("folds to the stored spelling"),
                path("projects/alpha")
            );
        } else {
            // Without folding the text is a word, and a capital makes the
            // match case sensitive.
            assert!(
                matches!(found, Err(ReferenceError::Unknown { .. })),
                "{found:?}"
            );
        }
    }

    #[test]
    fn the_live_set_folds_the_same_way() {
        let mut fx = Fixture::new();
        let folds = folds_case(&mut fx);
        fx.temp.tree().dir("projects/alpha");
        let set = CandidateSet::new(
            vec![path("projects/alpha"), path("projects/beta")],
            fx.root(),
        );
        let found = resolve_in(&fx, &set, fx.root().join("PROJECTS/alpha"));
        if folds {
            assert_eq!(
                found.expect("folds to the live spelling"),
                path("projects/alpha")
            );
        } else {
            assert!(
                matches!(found, Err(ReferenceError::Unknown { .. })),
                "{found:?}"
            );
        }
    }

    #[test]
    fn a_word_matching_one_piece_resolves_in_every_set() {
        let mut fx = Fixture::new();
        fx.temp.tree().dir("projects/alpha");
        fx.temp.tree().dir("projects/beta");
        store_version(&mut fx, "projects/alpha", &VersionFile::ALL);
        store_version(&mut fx, "projects/beta", &VersionFile::ALL);
        store_version(&mut fx, "old/zeta", &[VersionFile::Listing]);
        let selection = live_selection(&fx, &["projects/*"]);
        let found = scan(&fx.collection.pieces_dir()).expect("scans");
        for set in [
            CandidateSet::live(&fx.collection, &selection),
            CandidateSet::store(&fx.collection, &found),
            CandidateSet::store_and_sidecar_only(&fx.collection, &found),
        ] {
            assert_eq!(
                resolve_in(&fx, &set, "alp").expect("resolves"),
                path("projects/alpha")
            );
        }
    }

    #[test]
    fn two_matching_pieces_are_ambiguous_with_both_paths_in_the_json() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/alphabet", "projects/alpha", "other/x"]);
        let error = resolve_in(&fx, &set, "alph").expect_err("ambiguous");
        assert_eq!(kind(&error), ErrorKind::AmbiguousReference);
        let error: crate::error::Error = error.into();
        assert_eq!(
            serde_json::Value::Object(error.details()),
            serde_json::json!({"candidates": [
                {"path": "projects/alpha"},
                {"path": "projects/alphabet"},
            ]})
        );
    }

    #[test]
    fn a_word_matching_nothing_is_unknown() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/alpha"]);
        let error = resolve_in(&fx, &set, "zzz").expect_err("unknown");
        assert_eq!(kind(&error), ErrorKind::UnknownReference);
        assert_eq!(
            error.to_string(),
            "no piece matches `zzz`, as none of the pieces has a path that fits it"
        );
    }

    fn closest_of(set: &CandidateSet, given: &str) -> Vec<String> {
        let fx = Fixture::new();
        match resolve_in(&fx, set, given).expect_err("unknown") {
            ReferenceError::Unknown { closest, .. } => closest
                .into_iter()
                .map(|candidate| candidate.path)
                .collect(),
            other => panic!("expected an unknown reference, got {other}"),
        }
    }

    #[test]
    fn an_unknown_word_names_the_pieces_spelled_nearly_like_it() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/alpha", "projects/beta", "other/x"]);
        // Two letters swapped in the last name.
        assert_eq!(closest_of(&set, "alpah"), ["projects/alpha"]);
        // Two letters swapped in the whole path, which no fuzzy match finds.
        assert_eq!(closest_of(&set, "projetcs/alpha"), ["projects/alpha"]);
    }

    #[test]
    fn an_unknown_word_far_from_every_piece_has_no_closest_piece() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/alpha", "projects/beta"]);
        assert!(closest_of(&set, "zzzzzz").is_empty());
    }

    #[test]
    fn the_closest_pieces_are_at_most_three_nearest_first_then_by_path() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["a/scana", "b/scanb", "c/scanc", "d/scand", "e/scan"]);
        // `scanx` is one change from the four spelled `scan?` and one
        // insertion from `scan`; all five tie on distance one, so the path
        // order decides and the fourth is cut.
        assert_eq!(closest_of(&set, "scanx"), ["a/scana", "b/scanb", "c/scanc"]);
    }

    #[test]
    fn a_path_outside_the_collection_is_refused_through_resolve_piece() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/alpha"]);
        for given in [fx.outside.path().join("proj"), PathBuf::from("../x")] {
            let error = resolve_in(&fx, &set, given).expect_err("outside");
            assert_eq!(kind(&error), ErrorKind::OutsideCollection);
        }
    }

    #[test]
    fn a_sidecar_only_folder_is_a_candidate_of_forget_alone() {
        let mut fx = Fixture::new();
        store_version(&mut fx, "projects/alpha", &VersionFile::ALL);
        store_version(&mut fx, "projects/gone", &[VersionFile::Listing]);
        let found = scan(&fx.collection.pieces_dir()).expect("scans");
        let forget = CandidateSet::store_and_sidecar_only(&fx.collection, &found);
        let store = CandidateSet::store(&fx.collection, &found);
        assert_eq!(
            resolve_in(&fx, &forget, "gone").expect("resolves"),
            path("projects/gone")
        );
        let error = resolve_in(&fx, &store, "gone").expect_err("not a piece");
        assert_eq!(kind(&error), ErrorKind::UnknownReference);
    }

    // =====================================================
    // Search words for commands that remove something
    // =====================================================

    fn resolve_offering(
        fx: &Fixture,
        cwd: &Path,
        set: &CandidateSet,
        given: impl AsRef<OsStr>,
    ) -> Result<RelPath, ReferenceError> {
        resolve_piece(
            &fx.collection,
            cwd,
            given.as_ref(),
            set,
            SearchWord::OnlyOffers,
        )
    }

    #[test]
    fn a_word_matching_one_piece_is_only_offered_to_a_command_that_removes() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/alpha", "other/x"]);
        let error = resolve_offering(&fx, &fx.root(), &set, "alph").expect_err("only offered");
        assert_eq!(kind(&error), ErrorKind::InexactReference);
        let error: crate::error::Error = error.into();
        assert_eq!(
            serde_json::Value::Object(error.details()),
            serde_json::json!({"candidates": [{"path": "projects/alpha"}]})
        );
        assert_eq!(
            resolve_in(&fx, &set, "alph").expect("acts"),
            path("projects/alpha")
        );
    }

    #[test]
    fn the_exact_path_acts_for_a_command_that_removes() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/alpha", "projects/alphabet"]);
        assert_eq!(
            resolve_offering(&fx, &fx.root(), &set, "projects/alpha").expect("acts"),
            path("projects/alpha")
        );
        let from_below = fx.root().join("projects");
        assert_eq!(
            resolve_offering(&fx, &from_below, &set, "alpha").expect("acts"),
            path("projects/alpha")
        );
    }

    fn example_of(fx: &Fixture, cwd: &Path, set: &CandidateSet, word: &str) -> String {
        match resolve_offering(fx, cwd, set, word).expect_err("only offered") {
            ReferenceError::Inexact { example, .. } => example,
            other => unreachable!("only offered, not {other}"),
        }
    }

    #[test]
    fn the_example_is_the_path_below_the_root_and_absolute_elsewhere() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["tools/scan-old"]);
        assert_eq!(
            example_of(&fx, &fx.root(), &set, "scanold"),
            "tools/scan-old"
        );
        let from_below = fx.root().join("tools");
        let example = example_of(&fx, &from_below, &set, "scanold");
        assert_eq!(
            example,
            fx.root().join("tools/scan-old").to_string_lossy(),
            "absolute from a subdirectory"
        );
        let again = resolve_offering(&fx, &from_below, &set, &example).expect("acts");
        assert_eq!(again, path("tools/scan-old"));
    }

    #[test]
    fn the_example_of_a_name_with_a_percent_sign_is_its_raw_text() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["notes/100%25"]);
        let example = example_of(&fx, &fx.root(), &set, "notes");
        assert_eq!(example, "notes/100%");
        assert_eq!(
            resolve_offering(&fx, &fx.root(), &set, &example).expect("acts"),
            path("notes/100%25")
        );
    }

    #[test]
    fn the_example_of_a_name_that_is_not_utf8_is_its_escaped_form() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/d%FF"]);
        assert_eq!(
            example_of(&fx, &fx.root(), &set, "projects/d"),
            "projects/d%FF"
        );
        let from_below = fx.root().join("projects");
        let example = example_of(&fx, &from_below, &set, "projects/d");
        assert_eq!(
            example,
            format!("{}/projects/d%FF", escape(fx.root().as_os_str().as_bytes()))
        );
    }

    #[test]
    fn several_matches_and_no_match_are_reported_as_before_for_a_command_that_removes() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/alpha", "projects/alphabet"]);
        let error = resolve_offering(&fx, &fx.root(), &set, "alph").expect_err("several");
        assert_eq!(kind(&error), ErrorKind::AmbiguousReference);
        let error = resolve_offering(&fx, &fx.root(), &set, "zzz").expect_err("none");
        assert_eq!(kind(&error), ErrorKind::UnknownReference);
    }

    #[test]
    fn a_word_fitting_one_sidecar_only_folder_is_still_reported_as_incomplete() {
        let mut fx = Fixture::new();
        store_version(&mut fx, "projects/alpha", &VersionFile::ALL);
        store_version(&mut fx, "projects/gone", &[VersionFile::Listing]);
        let found = scan(&fx.collection.pieces_dir()).expect("scans");
        let error = resolve_stored_piece(
            &fx.collection,
            &fx.root(),
            OsStr::new("gone"),
            &found,
            SearchWord::OnlyOffers,
        )
        .expect_err("incomplete");
        assert!(
            matches!(error, ReferenceError::OnlyIncomplete { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_collection_root_is_a_word_not_a_candidate() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["v1.0/a", "v1.0/b", "plain"]);
        let error = resolve_in(&fx, &set, ".").expect_err("a dot matches two");
        assert!(
            matches!(error, ReferenceError::Ambiguous { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_dot_inside_a_piece_is_that_piece() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["v1.0/a", "v1.0/b"]);
        let cwd = fx.root().join("v1.0/a");
        let found = resolve_piece(
            &fx.collection,
            &cwd,
            OsStr::new("."),
            &set,
            SearchWord::Acts,
        );
        assert_eq!(found.expect("resolves"), path("v1.0/a"));
    }

    #[test]
    fn a_name_that_is_not_utf8_is_matched_in_its_escaped_form() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["projects/d%FF", "projects/other"]);
        let given = OsString::from_vec(b"d\xff".to_vec());
        assert_eq!(
            resolve_in(&fx, &set, &given).expect("resolves"),
            path("projects/d%FF")
        );
    }

    #[test]
    fn a_missing_pieces_directory_has_no_candidate() {
        let fx = Fixture::new();
        std::fs::remove_dir(fx.collection.pieces_dir()).expect("pieces/ is empty");
        let empty = set_of(&fx, &[]);
        let error = resolve_in(&fx, &empty, "Projects/alpha").expect_err("unknown");
        assert_eq!(kind(&error), ErrorKind::UnknownReference);
        // A set that names pieces over a directory that is gone must not
        // surface the missing directory either.
        let named = set_of(&fx, &["projects/alpha"]);
        let error = resolve_in(&fx, &named, "other/x").expect_err("unknown");
        assert_eq!(kind(&error), ErrorKind::UnknownReference);
    }

    #[test]
    fn a_directory_that_cannot_be_opened_is_an_error_of_kind_other() {
        let mut fx = Fixture::new();
        std::fs::remove_dir(fx.collection.pieces_dir()).expect("pieces/ is empty");
        fx.temp.tree().file(".antiq/pieces", b"not a directory");
        let set = set_of(&fx, &["projects/alpha"]);
        let error = resolve_in(&fx, &set, "other/x").expect_err("cannot open");
        assert!(matches!(error, ReferenceError::Open { .. }), "{error:?}");
        assert_eq!(kind(&error), ErrorKind::Other);
        assert!(error.to_string().contains("other/x"), "{error}");
    }

    #[test]
    fn a_lookup_that_fails_is_an_error_of_kind_other() {
        let mut fx = Fixture::new();
        fx.temp.tree().dir(".antiq/pieces/locked/inner");
        let locked = fx.collection.pieces_dir().join("locked");
        std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o000)).expect("mode");
        let _restore = RestoreModes(vec![locked]);
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let set = set_of(&fx, &["projects/alpha"]);
        let error = resolve_in(&fx, &set, "locked/inner").expect_err("cannot look");
        assert!(matches!(error, ReferenceError::Lookup { .. }), "{error:?}");
        assert_eq!(kind(&error), ErrorKind::Other);
        assert!(error.to_string().contains("locked/inner"), "{error}");
    }

    #[test]
    fn a_collection_root_that_cannot_be_looked_at_is_an_error_through_resolve_piece() {
        let fx = Fixture::new();
        let gone = Collection::for_tests();
        let set = set_of(&fx, &["x"]);
        let error = resolve_piece(&gone, &fx.root(), OsStr::new("x"), &set, SearchWord::Acts)
            .expect_err("no root");
        assert!(matches!(error, ReferenceError::Root { .. }), "{error:?}");
    }

    #[test]
    fn a_current_directory_that_is_not_absolute_is_a_path_error_through_resolve_piece() {
        let fx = Fixture::new();
        let set = set_of(&fx, &["x"]);
        let error = resolve_piece(
            &fx.collection,
            Path::new("rel"),
            OsStr::new("x"),
            &set,
            SearchWord::Acts,
        )
        .expect_err("not absolute");
        assert!(matches!(error, ReferenceError::Path(_)), "{error:?}");
    }

    // The stored spelling of a path that may not exist yet.

    fn pieces_root(fx: &Fixture) -> TreeRoot {
        TreeRoot::open(&fx.collection.pieces_dir()).expect("pieces/ opens")
    }

    #[test]
    fn a_new_folder_below_an_existing_parent_takes_the_stored_spelling() {
        let mut fx = Fixture::new();
        let folds = folds_case(&mut fx);
        fx.temp.tree().dir(".antiq/pieces/projects");
        let found = stored_spelling(&pieces_root(&fx), &path("Projects/New")).expect("answers");
        if folds {
            assert_eq!(found, path("projects/New"));
        } else {
            assert_eq!(found, path("Projects/New"));
        }
    }

    #[test]
    fn an_existing_path_gives_its_stored_spelling() {
        let mut fx = Fixture::new();
        let folds = folds_case(&mut fx);
        fx.temp.tree().dir(".antiq/pieces/old/client");
        let found = stored_spelling(&pieces_root(&fx), &path("OLD/Client")).expect("answers");
        assert_eq!(
            found,
            if folds {
                path("old/client")
            } else {
                path("OLD/Client")
            }
        );
    }

    #[test]
    fn a_path_with_no_existing_prefix_and_the_root_are_unchanged() {
        let fx = Fixture::new();
        let root = pieces_root(&fx);
        assert_eq!(
            stored_spelling(&root, &path("No/Where/Yet")).expect("answers"),
            path("No/Where/Yet")
        );
        assert_eq!(
            stored_spelling(&root, &RelPath::root()).expect("answers"),
            RelPath::root()
        );
    }

    #[test]
    fn a_failing_look_while_finding_the_spelling_is_an_error() {
        let mut fx = Fixture::new();
        fx.temp.tree().dir(".antiq/pieces/locked/inner");
        let locked = fx.collection.pieces_dir().join("locked");
        std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o000)).expect("mode");
        let _restore = RestoreModes(vec![locked]);
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let error =
            stored_spelling(&pieces_root(&fx), &path("locked/inner/x")).expect_err("cannot look");
        assert!(matches!(error, ReferenceError::Lookup { .. }), "{error:?}");
    }
}

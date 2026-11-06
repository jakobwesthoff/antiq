// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Devices, mount points and filesystem kinds.

// TODO: the piece walk (I05) and the collection and remote checks (I16,
// I23) are the callers; until they land the library has none.
#![allow(dead_code)]

use super::{FsxError, io_error};
use crate::fsx::tree::TreeRoot;
use crate::relpath::RelPath;
use std::io;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::SystemTime;

/// The device a directory lives on, in the form `TreeEntry::device` has,
/// so the two can be compared. `path` must be a readable directory.
pub(crate) fn device_of(path: &Path) -> Result<u64, FsxError> {
    identity_of(path)
        .map(|(device, _)| device)
        .map_err(io_error("stat", path))
}

/// Whether another filesystem is mounted at `path`.
pub(crate) fn is_mount_point(path: &Path) -> Result<bool, FsxError> {
    is_mount_point_with(path, &identity_of, &named_by_platform)
}

/// Device and inode of a directory.
type Identity = (u64, u64);

/// The device is read the way the walk reads it, so the two compare.
fn identity_of(path: &Path) -> io::Result<Identity> {
    let entry = TreeRoot::open(path)?.lstat(&RelPath::root())?;
    Ok((entry.device, entry.inode))
}

/// A directory is a mount point when its device differs from its
/// parent's, or when it is its own parent (the root). Bind mounts and
/// the macOS data volume share the device of their parent, so
/// `by_platform` asks the operating system as well.
fn is_mount_point_with(
    path: &Path,
    identity: &dyn Fn(&Path) -> io::Result<Identity>,
    by_platform: &dyn Fn(&Path) -> io::Result<bool>,
) -> Result<bool, FsxError> {
    let own = identity(path).map_err(io_error("stat", path))?;
    let parent_path = path.join("..");
    let parent = identity(&parent_path).map_err(io_error("stat", &parent_path))?;
    if own.0 != parent.0 || own == parent {
        return Ok(true);
    }
    by_platform(path).map_err(io_error("stat", path))
}

/// The macOS mount table names the exact mount point of a path.
#[cfg(target_os = "macos")]
fn named_by_platform(path: &Path) -> io::Result<bool> {
    let stat = rustix::fs::statfs(path)?;
    let mount_name: Vec<u8> = stat
        .f_mntonname
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    let canonical = std::fs::canonicalize(path)?;
    Ok(canonical.as_os_str().as_bytes() == mount_name.as_slice())
}

/// Linux marks the root of every mount, bind mounts included, in the
/// attributes of `statx`, which the walk's `lstat` reads into
/// `TreeEntry::mount_root`.
#[cfg(target_os = "linux")]
fn named_by_platform(path: &Path) -> io::Result<bool> {
    Ok(TreeRoot::open(path)?.lstat(&RelPath::root())?.mount_root)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn named_by_platform(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

/// What kind of filesystem a path lives on, as far as antiq treats them
/// differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsKind {
    Local,
    Smb,
    Nfs,
    Other,
}

/// The kind named by the type name macOS reports (`f_fstypename`).
fn kind_from_name(name: &str) -> FsKind {
    match name {
        "apfs" | "hfs" => FsKind::Local,
        "smbfs" => FsKind::Smb,
        "nfs" => FsKind::Nfs,
        _ => FsKind::Other,
    }
}

/// The kind named by the type magic number Linux reports (`f_type`):
/// ext2 to ext4, btrfs, tmpfs and overlayfs; SMB, CIFS and SMB2; NFS.
fn kind_from_magic(magic: u32) -> FsKind {
    match magic {
        0xef53 | 0x9123_683e | 0x0102_1994 | 0x794c_7630 => FsKind::Local,
        0x517b | 0xff53_4d42 | 0xfe53_4d42 => FsKind::Smb,
        0x6969 => FsKind::Nfs,
        _ => FsKind::Other,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn fs_kind(path: &Path) -> Result<FsKind, FsxError> {
    let stat = rustix::fs::statfs(path).map_err(|errno| io_error("stat", path)(errno.into()))?;
    let name: Vec<u8> = stat
        .f_fstypename
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    Ok(kind_from_name(&String::from_utf8_lossy(&name)))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn fs_kind(path: &Path) -> Result<FsKind, FsxError> {
    let stat = rustix::fs::statfs(path).map_err(|errno| io_error("stat", path)(errno.into()))?;
    // Every magic number of `kind_from_magic` fits 32 bits, and the
    // field's own width differs between platforms.
    Ok(kind_from_magic(stat.f_type as u32))
}

/// The birth time of the entry itself in nanoseconds since the epoch
/// (negative before it), or `None` where the filesystem keeps none.
pub(crate) fn birth_time(path: &Path) -> Result<Option<i128>, FsxError> {
    let metadata = std::fs::symlink_metadata(path).map_err(io_error("stat", path))?;
    birth_from(metadata.created()).map_err(io_error("stat", path))
}

/// `Unsupported` is how std says a platform or filesystem keeps no birth
/// time (musl does not).
fn birth_from(created: io::Result<SystemTime>) -> io::Result<Option<i128>> {
    match created {
        Ok(time) => Ok(Some(nanoseconds_since_epoch(time))),
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(None),
        Err(error) => Err(error),
    }
}

fn nanoseconds_since_epoch(time: SystemTime) -> i128 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => after.as_nanos() as i128,
        Err(before) => -(before.duration().as_nanos() as i128),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::Tree;

    fn here() -> i128 {
        nanoseconds_since_epoch(SystemTime::now())
    }

    #[test]
    fn the_device_is_the_one_the_walk_reports() {
        let tree = Tree::new();
        let expected = TreeRoot::open(tree.root())
            .expect("opens")
            .lstat(&RelPath::root())
            .expect("stat")
            .device;
        assert_ne!(expected, 0, "the test needs a device number");
        assert_eq!(device_of(tree.root()).expect("device"), expected);
    }

    #[test]
    fn the_device_of_a_missing_path_is_an_error() {
        let tree = Tree::new();
        let error = device_of(&tree.path("nope")).expect_err("fails");
        assert!(error.to_string().contains("nope"), "{error}");
    }

    fn mount_point(own: Identity, parent: Identity, platform: bool) -> bool {
        let identity = |path: &Path| Ok(if path.ends_with("..") { parent } else { own });
        is_mount_point_with(Path::new("/x"), &identity, &|_| Ok(platform)).expect("answers")
    }

    #[test]
    fn another_device_than_the_parent_makes_a_mount_point() {
        assert!(mount_point((7, 10), (8, 20), false));
    }

    #[test]
    fn the_same_device_and_another_inode_is_no_mount_point() {
        assert!(!mount_point((7, 10), (7, 20), false));
    }

    #[test]
    fn a_directory_that_is_its_own_parent_is_a_mount_point() {
        assert!(mount_point((7, 2), (7, 2), false));
    }

    #[test]
    fn the_platform_can_name_a_mount_point_on_the_parents_device() {
        assert!(mount_point((7, 10), (7, 20), true));
    }

    #[test]
    fn a_failing_stat_is_an_error_naming_the_path() {
        let identity = |_: &Path| Err(io::Error::from_raw_os_error(2));
        let error =
            is_mount_point_with(Path::new("/x"), &identity, &|_| Ok(false)).expect_err("fails");
        assert!(error.to_string().contains("/x"), "{error}");
    }

    #[test]
    fn the_root_is_a_mount_point_and_a_temp_directory_is_not() {
        assert!(is_mount_point(Path::new("/")).expect("answers"));
        let tree = Tree::new();
        assert!(!is_mount_point(tree.root()).expect("answers"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_data_volume_is_a_mount_point_although_it_shares_its_parents_device() {
        use std::os::unix::fs::MetadataExt;
        let data = Path::new("/System/Volumes/Data");
        let parent = std::fs::metadata("/System/Volumes").expect("the parent exists");
        assert_eq!(
            std::fs::metadata(data).expect("exists").dev(),
            parent.dev(),
            "the test needs the data volume to share its parent's device"
        );
        assert!(is_mount_point(data).expect("answers"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_is_a_mount_point() {
        assert!(is_mount_point(Path::new("/proc")).expect("answers"));
    }

    /// Whether the run may mount: only the privileged Linux container sets
    /// `ANTIQ_PRIVILEGED_TESTS=1`. Elsewhere the mount tests report that
    /// they skipped and pass, so a run with the ignored tests included does
    /// not fail on a machine that cannot mount. Where the variable is set,
    /// a mount that fails fails the test.
    #[cfg(target_os = "linux")]
    fn privileged() -> bool {
        let allowed = std::env::var("ANTIQ_PRIVILEGED_TESTS").is_ok_and(|value| value == "1");
        if !allowed {
            eprintln!("skipped: set ANTIQ_PRIVILEGED_TESTS=1 as root to run the mount tests");
        }
        allowed
    }

    /// A bind mount of `source` at `target`, undone when dropped. Mounting
    /// needs CAP_SYS_ADMIN, which the privileged Linux container has.
    #[cfg(target_os = "linux")]
    struct BindMount(std::path::PathBuf);

    #[cfg(target_os = "linux")]
    impl BindMount {
        fn new(source: &Path, target: &Path) -> BindMount {
            let status = std::process::Command::new("mount")
                .arg("--bind")
                .arg(source)
                .arg(target)
                .status()
                .expect("mount runs");
            assert!(
                status.success(),
                "the test needs the right to mount: run it as root in a privileged container"
            );
            BindMount(target.to_path_buf())
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for BindMount {
        fn drop(&mut self) {
            // Best effort: a mount left behind goes with the container.
            let _ = std::process::Command::new("umount").arg(&self.0).status();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_bind_mount_is_a_mount_root_on_its_parents_device() {
        if !privileged() {
            return;
        }
        use std::os::unix::fs::MetadataExt;
        let mut tree = Tree::new();
        tree.file("src/precious", "x");
        tree.dir("p/mnt");
        let _mount = BindMount::new(&tree.path("src"), &tree.path("p/mnt"));

        let parent_device = std::fs::metadata(tree.path("p")).expect("exists").dev();
        let mount_device = std::fs::metadata(tree.path("p/mnt")).expect("exists").dev();
        assert_eq!(
            mount_device, parent_device,
            "the test needs the bind mount to share its parent's device"
        );
        let root = TreeRoot::open(tree.root()).expect("opens");
        let path = |text: &str| RelPath::parse(text).expect("valid path");
        assert!(root.lstat(&path("p/mnt")).expect("stat").mount_root);
        assert!(!root.lstat(&path("p")).expect("stat").mount_root);
        assert!(is_mount_point(&tree.path("p/mnt")).expect("answers"));
        assert!(!is_mount_point(&tree.path("p")).expect("answers"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn removing_a_tree_stops_at_a_bind_mount_and_leaves_its_source_alone() {
        if !privileged() {
            return;
        }
        use crate::fsx::{FsxError, remove::remove_tree};
        let mut tree = Tree::new();
        tree.file("src/precious", "x");
        tree.file("p/before", "y");
        tree.dir("p/mnt");
        let _mount = BindMount::new(&tree.path("src"), &tree.path("p/mnt"));

        let root = TreeRoot::open(tree.root()).expect("opens");
        let error = remove_tree(&root, &RelPath::parse("p").expect("valid")).expect_err("stops");
        assert!(
            matches!(&error, FsxError::ForeignDevice { path } if path == "p/mnt"),
            "{error}"
        );
        assert!(tree.path("src/precious").exists(), "the source was emptied");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_walk_reports_a_bind_mount_inside_a_piece_and_as_a_piece() {
        if !privileged() {
            return;
        }
        use crate::select::Rules;
        use crate::tree::Problem;
        use crate::walk::walk_piece;
        let mut tree = Tree::new();
        tree.file("src/precious", "x");
        tree.file("p/before", "y");
        tree.dir("p/mnt");
        tree.dir("bound");
        let _inside = BindMount::new(&tree.path("src"), &tree.path("p/mnt"));
        let _as_piece = BindMount::new(&tree.path("src"), &tree.path("bound"));

        let root = TreeRoot::open(tree.root()).expect("opens");
        let rules = Rules::new(&[], &[]).expect("no patterns are valid");
        let path = |text: &str| RelPath::parse(text).expect("valid path");
        let inside = walk_piece(&root, &path("p"), &[], &rules).expect("walks");
        assert_eq!(
            inside.problems(),
            [Problem::MountPoint { other: path("mnt") }]
        );
        let as_piece = walk_piece(&root, &path("bound"), &[], &rules).expect("walks");
        assert_eq!(
            as_piece.problems(),
            [Problem::MountPoint {
                other: RelPath::root()
            }]
        );
    }

    #[test]
    fn macos_type_names_map_to_kinds() {
        for (name, kind) in [
            ("apfs", FsKind::Local),
            ("hfs", FsKind::Local),
            ("smbfs", FsKind::Smb),
            ("nfs", FsKind::Nfs),
            ("exfat", FsKind::Other),
            ("", FsKind::Other),
        ] {
            assert_eq!(kind_from_name(name), kind, "{name}");
        }
    }

    #[test]
    fn linux_magic_numbers_map_to_kinds() {
        for (magic, kind) in [
            (0xef53, FsKind::Local),
            (0x9123_683e, FsKind::Local),
            (0x0102_1994, FsKind::Local),
            (0x794c_7630, FsKind::Local),
            (0x517b, FsKind::Smb),
            (0xff53_4d42, FsKind::Smb),
            (0xfe53_4d42, FsKind::Smb),
            (0x6969, FsKind::Nfs),
            (0x5846_5342, FsKind::Other),
            (0, FsKind::Other),
        ] {
            assert_eq!(kind_from_magic(magic), kind, "{magic:#x}");
        }
    }

    #[test]
    fn a_temp_directory_is_not_on_a_network_filesystem() {
        let tree = Tree::new();
        let kind = fs_kind(tree.root()).expect("answers");
        assert!(
            !matches!(kind, FsKind::Smb | FsKind::Nfs),
            "the test needs a local temp directory, found {kind:?}"
        );
        #[cfg(target_os = "macos")]
        assert_eq!(kind, FsKind::Local);
    }

    #[test]
    fn the_kind_of_a_missing_path_is_an_error() {
        let tree = Tree::new();
        assert!(fs_kind(&tree.path("nope")).is_err());
    }

    #[test]
    fn a_new_file_was_born_a_moment_ago() {
        let mut tree = Tree::new();
        let file = tree.file("f", "x");
        let born = birth_time(&file).expect("answers");
        #[cfg(target_os = "macos")]
        assert!(born.is_some(), "macOS keeps birth times");
        let minute = 60 * 1_000_000_000;
        let now = here();
        assert!(born.is_none_or(|born| (now - born).abs() < minute));
    }

    #[test]
    fn unsupported_means_no_birth_time_and_other_errors_stay() {
        let unsupported = Err(io::Error::from(io::ErrorKind::Unsupported));
        assert_eq!(birth_from(unsupported).expect("no error"), None);
        let denied = Err(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(birth_from(denied).is_err());
    }

    #[test]
    fn the_birth_time_of_a_symlink_is_its_own() {
        let mut tree = Tree::new();
        let link = tree.symlink("l", "nowhere");
        assert!(
            birth_time(&link).is_ok(),
            "a dangling link has a birth time"
        );
    }

    #[test]
    fn the_birth_time_of_a_missing_path_is_an_error() {
        let tree = Tree::new();
        assert!(birth_time(&tree.path("nope")).is_err());
    }

    #[test]
    fn times_before_the_epoch_are_negative() {
        let before = SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
        assert_eq!(nanoseconds_since_epoch(before), -1_000_000_000);
        let after = SystemTime::UNIX_EPOCH + std::time::Duration::new(2, 5);
        assert_eq!(nanoseconds_since_epoch(after), 2_000_000_005);
    }
}

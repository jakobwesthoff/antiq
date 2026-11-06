// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `[target] url`: where `push` mirrors the collection to.

use crate::error::HasKind;
use std::ffi::OsString;
use std::path::PathBuf;

/// Why a text is no target URL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("the target URL is empty")]
    Empty,
    #[error("`{text}` is neither an absolute path nor [user@]host:path")]
    Form { text: String },
    #[error("the target URL holds a NUL byte, which no path or argument can carry")]
    Nul,
    #[error("`{text}` starts with `-`, which rsync would read as an option")]
    HostOption { text: String },
    #[error("the target URL names no host")]
    NoHost,
    #[error("the target URL has an empty user before `@`")]
    NoUser,
    #[error("the target URL has no path after the host")]
    NoPath,
    #[error("the host `{text}` holds a `/`, which rsync would read as a local path")]
    HostSlash { text: String },
    #[error("the brackets of the host in `{text}` do not enclose it as `[host]:`")]
    Brackets { text: String },
    #[error("the path `{text}` starts with `:` or `//`, which rsync reads as daemon syntax")]
    Daemon { text: String },
    #[error(
        "the path `{text}` holds `*`, `?`, `[` or `]`, which the shell on the target would expand"
    )]
    Wildcard { text: String },
}

impl HasKind for UrlError {}

/// A target URL taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetUrl {
    /// An absolute local path, such as the mount point of a share.
    Path { root: PathBuf },
    /// `[user@]host:path`, with the path as rsync is to receive it. An
    /// IPv6 host keeps its brackets.
    Ssh {
        user: Option<String>,
        host: String,
        path: String,
    },
}

impl TargetUrl {
    /// Takes `text` apart. The text is not normalised: the config keeps it
    /// as given.
    pub fn parse(text: &str) -> Result<TargetUrl, UrlError> {
        if text.is_empty() {
            return Err(UrlError::Empty);
        }
        if text.contains('\0') {
            return Err(UrlError::Nul);
        }
        // The text ends up as one rsync argument; a leading dash would make
        // it an option, whatever follows.
        if text.starts_with('-') {
            return Err(UrlError::HostOption { text: text.into() });
        }
        if text.starts_with('/') {
            return Ok(TargetUrl::Path { root: text.into() });
        }
        parse_ssh(text)
    }

    /// The target directory as rsync takes it, with one trailing `/` so
    /// that rsync works on the directory's content.
    pub fn dir_arg(&self) -> OsString {
        let mut arg = match self {
            TargetUrl::Path { root } => root.as_os_str().to_os_string(),
            TargetUrl::Ssh { user, host, path } => {
                let user = user.as_ref().map(|user| format!("{user}@"));
                format!("{}{host}:{path}", user.unwrap_or_default()).into()
            }
        };
        if !arg.as_encoded_bytes().ends_with(b"/") {
            arg.push("/");
        }
        arg
    }
}

/// Takes `[user@]host:path` apart. A host in brackets may hold colons, so
/// the host ends at the first colon outside brackets.
fn parse_ssh(text: &str) -> Result<TargetUrl, UrlError> {
    let form = || UrlError::Form { text: text.into() };
    let brackets = || UrlError::Brackets { text: text.into() };
    let boundary = text.find(['[', ':']).ok_or_else(form)?;
    let (user, host, path) = if text[boundary..].starts_with('[') {
        let user = match &text[..boundary] {
            "" => None,
            before => Some(before.strip_suffix('@').ok_or_else(brackets)?),
        };
        let close = text[boundary..].find(']').ok_or_else(brackets)? + boundary;
        let path = text[close + 1..].strip_prefix(':').ok_or_else(brackets)?;
        (user, &text[boundary..=close], path)
    } else {
        let (user, host) = match text[..boundary].rsplit_once('@') {
            Some((user, host)) => (Some(user), host),
            None => (None, &text[..boundary]),
        };
        (user, host, &text[boundary + 1..])
    };
    if user == Some("") {
        return Err(UrlError::NoUser);
    }
    if host.is_empty() || host == "[]" {
        return Err(UrlError::NoHost);
    }
    if host.starts_with('-') {
        return Err(UrlError::HostOption { text: host.into() });
    }
    // rsync takes an argument with a slash before its first colon for a
    // local relative path.
    if host.contains('/') {
        return Err(UrlError::HostSlash { text: host.into() });
    }
    if path.is_empty() {
        return Err(UrlError::NoPath);
    }
    // `host::module` and `rsync://host/module` make rsync speak to a daemon
    // instead of running ssh.
    if path.starts_with(':') || path.starts_with("//") {
        return Err(UrlError::Daemon { text: path.into() });
    }
    // rsync leaves these in a remote path to be expanded on the target, so
    // the listing and the deletion could hit different directories.
    if path.contains(['*', '?', '[', ']']) {
        return Err(UrlError::Wildcard { text: path.into() });
    }
    Ok(TargetUrl::Ssh {
        user: user.map(String::from),
        host: host.into(),
        path: path.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(user: Option<&str>, host: &str, path: &str) -> TargetUrl {
        TargetUrl::Ssh {
            user: user.map(String::from),
            host: host.into(),
            path: path.into(),
        }
    }

    fn path(root: &str) -> TargetUrl {
        TargetUrl::Path { root: root.into() }
    }

    fn refused(text: &str) -> UrlError {
        TargetUrl::parse(text).expect_err("the text is refused")
    }

    #[test]
    fn an_absolute_path_may_hold_spaces_colons_and_non_ascii() {
        let text = "/Volumes/My Share: 2/Büro \"x\"";
        assert_eq!(TargetUrl::parse(text), Ok(path(text)));
    }

    #[test]
    fn a_leading_slash_means_a_path_even_with_a_colon_in_the_first_name() {
        assert_eq!(TargetUrl::parse("/a:b"), Ok(path("/a:b")));
    }

    #[test]
    fn the_filesystem_root_parses_as_a_path() {
        assert_eq!(TargetUrl::parse("/"), Ok(path("/")));
    }

    #[test]
    fn host_and_path_parse() {
        assert_eq!(
            TargetUrl::parse("nas.local:/volume1/antiq"),
            Ok(ssh(None, "nas.local", "/volume1/antiq"))
        );
    }

    #[test]
    fn a_path_without_a_slash_in_front_stays_as_written() {
        assert_eq!(
            TargetUrl::parse("nas:backups/antiq"),
            Ok(ssh(None, "nas", "backups/antiq"))
        );
    }

    #[test]
    fn user_host_and_a_path_with_a_space_parse() {
        assert_eq!(
            TargetUrl::parse("jakob@nas:/vol/q r"),
            Ok(ssh(Some("jakob"), "nas", "/vol/q r"))
        );
    }

    #[test]
    fn an_at_sign_in_the_path_is_not_the_user_separator() {
        assert_eq!(TargetUrl::parse("nas:dir@x"), Ok(ssh(None, "nas", "dir@x")));
    }

    #[test]
    fn a_bracketed_ipv6_host_keeps_its_brackets() {
        assert_eq!(
            TargetUrl::parse("[::1]:/srv/a"),
            Ok(ssh(None, "[::1]", "/srv/a"))
        );
        assert_eq!(
            TargetUrl::parse("u@[fe80::1]:x"),
            Ok(ssh(Some("u"), "[fe80::1]", "x"))
        );
    }

    #[test]
    fn unbalanced_or_misplaced_brackets_are_refused() {
        for text in ["[::1:/x", "[::1]x", "ho[st]:/x"] {
            assert!(matches!(refused(text), UrlError::Brackets { .. }), "{text}");
        }
    }

    #[test]
    fn a_host_starting_with_a_dash_is_refused() {
        for text in ["-host:p", "user@-host:p", "-u@host:p"] {
            assert!(
                matches!(refused(text), UrlError::HostOption { .. }),
                "{text}"
            );
        }
    }

    #[test]
    fn text_without_a_leading_slash_and_without_a_colon_is_refused() {
        for text in ["backups/x", "./x", "~/x", "host"] {
            assert!(matches!(refused(text), UrlError::Form { .. }), "{text}");
        }
    }

    #[test]
    fn empty_text_is_refused() {
        assert_eq!(refused(""), UrlError::Empty);
    }

    #[test]
    fn a_missing_host_user_or_path_is_refused_by_its_own_error() {
        assert_eq!(refused(":p"), UrlError::NoHost);
        assert_eq!(refused("u@:p"), UrlError::NoHost);
        assert_eq!(refused("@h:p"), UrlError::NoUser);
        assert_eq!(refused("h:"), UrlError::NoPath);
    }

    #[test]
    fn a_host_with_a_slash_is_refused() {
        assert!(matches!(refused("dir/a:b"), UrlError::HostSlash { .. }));
    }

    #[test]
    fn daemon_syntax_is_refused() {
        for text in ["rsync://h/x", "ssh://h/p", "host::module", "h:://x"] {
            assert!(matches!(refused(text), UrlError::Daemon { .. }), "{text}");
        }
    }

    #[test]
    fn wildcard_characters_in_an_ssh_path_are_refused() {
        for text in [
            "h:/a*b",
            "h:/a?b",
            "h:/a[1]",
            "h:/a]",
            "u@h:*",
            "[::1]:/a[1]",
        ] {
            assert!(matches!(refused(text), UrlError::Wildcard { .. }), "{text}");
        }
    }

    #[test]
    fn brackets_around_the_host_and_wildcards_in_a_local_path_are_no_wildcards() {
        assert_eq!(
            TargetUrl::parse("[::1]:/srv/a"),
            Ok(ssh(None, "[::1]", "/srv/a"))
        );
        assert_eq!(
            TargetUrl::parse("u@[fe80::1]:x"),
            Ok(ssh(Some("u"), "[fe80::1]", "x"))
        );
        assert_eq!(
            TargetUrl::parse("/Volumes/a[1]*"),
            Ok(path("/Volumes/a[1]*"))
        );
    }

    #[test]
    fn a_nul_is_refused() {
        assert_eq!(refused("/a\0b"), UrlError::Nul);
        assert_eq!(refused("h:a\0b"), UrlError::Nul);
    }

    #[test]
    fn the_directory_argument_ends_in_one_slash() {
        let args = [
            ("/srv/antiq", "/srv/antiq/"),
            ("/srv/antiq/", "/srv/antiq/"),
            ("/", "/"),
            ("nas:/vol/a", "nas:/vol/a/"),
            ("u@nas:rel dir/", "u@nas:rel dir/"),
            ("u@[::1]:x", "u@[::1]:x/"),
        ];
        for (text, expected) in args {
            let url = TargetUrl::parse(text).expect("the text parses");
            assert_eq!(url.dir_arg(), OsString::from(expected), "{text}");
        }
    }
}

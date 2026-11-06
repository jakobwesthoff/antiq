// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The URL of a remote, as given to `remote add`.

use crate::error::HasKind;
use std::path::PathBuf;

/// Why a text is no remote URL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("the remote URL is empty")]
    Empty,
    #[error("`{text}` is neither an absolute path nor an ssh://[user@]host[:port]/<path> URL")]
    Form { text: String },
    #[error("the root path may contain only A-Z a-z 0-9 . _ / -, found {found:?}")]
    Character { found: char },
    #[error("the root path has an empty component")]
    EmptyComponent,
    #[error("the root path has the component `{component}`")]
    DotComponent { component: String },
    #[error("the root path ends with `/`")]
    TrailingSlash,
    #[error("the root path is the filesystem root")]
    BareRoot,
    #[error("the ssh URL has no host")]
    NoHost,
    #[error("the ssh URL has no path")]
    NoPath,
    #[error("`{text}` is not a user or host name of A-Z a-z 0-9 . _ -")]
    Name { text: String },
    #[error("`{text}` is not a port from 1 to 65535")]
    Port { text: String },
}

impl HasKind for UrlError {}

/// A remote's URL taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteUrl {
    /// An absolute local path, such as the mount point of a share.
    Path { root: PathBuf },
    /// `ssh://[user@]host[:port]/<path>`.
    Ssh {
        user: Option<String>,
        host: String,
        port: Option<u16>,
        root: String,
    },
}

impl RemoteUrl {
    /// Takes `text` apart. The text is not normalised: the config keeps it
    /// as given.
    pub fn parse(text: &str) -> Result<RemoteUrl, UrlError> {
        if text.is_empty() {
            return Err(UrlError::Empty);
        }
        if let Some(rest) = text.strip_prefix("ssh://") {
            return parse_ssh(rest);
        }
        if text.starts_with('/') {
            check_root(text)?;
            return Ok(RemoteUrl::Path { root: text.into() });
        }
        Err(UrlError::Form { text: text.into() })
    }

    /// The kind `remote show` prints.
    pub fn kind(&self) -> &'static str {
        match self {
            RemoteUrl::Path { .. } => "path",
            RemoteUrl::Ssh { .. } => "ssh",
        }
    }
}

/// The part of an ssh URL after `ssh://`.
fn parse_ssh(rest: &str) -> Result<RemoteUrl, UrlError> {
    let (authority, root) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, ""),
    };
    let (user, host_port) = match authority.rsplit_once('@') {
        Some((user, host_port)) => (Some(user), host_port),
        None => (None, authority),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (host_port, None),
    };
    if host.is_empty() {
        return Err(UrlError::NoHost);
    }
    if root.is_empty() {
        return Err(UrlError::NoPath);
    }
    for name in user.into_iter().chain([host]) {
        if name.is_empty() || !name.bytes().all(is_name_byte) {
            return Err(UrlError::Name { text: name.into() });
        }
    }
    let port = port
        .map(|text| {
            text.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| UrlError::Port { text: text.into() })
        })
        .transpose()?;
    check_root(root)?;
    Ok(RemoteUrl::Ssh {
        user: user.map(String::from),
        host: host.into(),
        port,
        root: root.into(),
    })
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

/// Checks a root path that starts with `/`: only the bytes
/// `[A-Za-z0-9._/-]`, no empty or dot components, no trailing `/`, and not
/// the filesystem root. Two spellings of one directory would otherwise
/// both be accepted, and a last component that is `..` has no name to
/// create.
fn check_root(root: &str) -> Result<(), UrlError> {
    if let Some(found) = root
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-')))
    {
        return Err(UrlError::Character { found });
    }
    if root == "/" {
        return Err(UrlError::BareRoot);
    }
    if root.ends_with('/') {
        return Err(UrlError::TrailingSlash);
    }
    for component in root[1..].split('/') {
        match component {
            "" => return Err(UrlError::EmptyComponent),
            "." | ".." => {
                return Err(UrlError::DotComponent {
                    component: component.into(),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<RemoteUrl, UrlError> {
        RemoteUrl::parse(text)
    }

    fn ssh(user: Option<&str>, host: &str, port: Option<u16>, root: &str) -> RemoteUrl {
        RemoteUrl::Ssh {
            user: user.map(String::from),
            host: host.into(),
            port,
            root: root.into(),
        }
    }

    #[test]
    fn an_absolute_path_is_a_path_remote() {
        assert_eq!(
            parse("/Volumes/nas/antiq-1.x_y"),
            Ok(RemoteUrl::Path {
                root: "/Volumes/nas/antiq-1.x_y".into()
            })
        );
    }

    #[test]
    fn the_kind_names_the_form() {
        assert_eq!(parse("/a").expect("valid").kind(), "path");
        assert_eq!(parse("ssh://h/a").expect("valid").kind(), "ssh");
    }

    #[test]
    fn an_empty_text_is_refused() {
        assert_eq!(parse(""), Err(UrlError::Empty));
    }

    #[test]
    fn a_relative_path_is_refused() {
        assert_eq!(
            parse("nas/antiq"),
            Err(UrlError::Form {
                text: "nas/antiq".into()
            })
        );
    }

    #[test]
    fn another_scheme_is_refused() {
        assert!(matches!(
            parse("https://host/antiq"),
            Err(UrlError::Form { .. })
        ));
    }

    #[test]
    fn a_byte_outside_the_root_alphabet_is_refused_and_named() {
        for (text, found) in [
            ("/a b", ' '),
            ("/a%20b", '%'),
            ("/~/a", '~'),
            ("/Müll", 'ü'),
        ] {
            assert_eq!(parse(text), Err(UrlError::Character { found }), "{text}");
        }
    }

    #[test]
    fn an_empty_component_is_refused() {
        assert_eq!(parse("/a//b"), Err(UrlError::EmptyComponent));
    }

    #[test]
    fn a_dot_component_is_refused() {
        assert_eq!(
            parse("/a/./b"),
            Err(UrlError::DotComponent {
                component: ".".into()
            })
        );
    }

    #[test]
    fn a_dot_dot_component_is_refused() {
        assert_eq!(
            parse("/mnt/nas/new/.."),
            Err(UrlError::DotComponent {
                component: "..".into()
            })
        );
    }

    #[test]
    fn a_trailing_slash_is_refused() {
        assert_eq!(parse("/a/b/"), Err(UrlError::TrailingSlash));
    }

    #[test]
    fn the_filesystem_root_is_refused() {
        assert_eq!(parse("/"), Err(UrlError::BareRoot));
    }

    #[test]
    fn an_ssh_url_has_a_host_and_a_path() {
        assert_eq!(
            parse("ssh://nas/srv/antiq"),
            Ok(ssh(None, "nas", None, "/srv/antiq"))
        );
    }

    #[test]
    fn an_ssh_url_may_carry_a_user() {
        assert_eq!(
            parse("ssh://jw@nas.local/srv/antiq"),
            Ok(ssh(Some("jw"), "nas.local", None, "/srv/antiq"))
        );
    }

    #[test]
    fn an_ssh_url_may_carry_a_port() {
        assert_eq!(
            parse("ssh://nas:2222/srv/antiq"),
            Ok(ssh(None, "nas", Some(2222), "/srv/antiq"))
        );
    }

    #[test]
    fn an_ssh_url_may_carry_both() {
        assert_eq!(
            parse("ssh://jw@nas:22/srv"),
            Ok(ssh(Some("jw"), "nas", Some(22), "/srv"))
        );
    }

    #[test]
    fn an_ssh_url_without_a_path_is_refused() {
        assert_eq!(parse("ssh://nas"), Err(UrlError::NoPath));
    }

    #[test]
    fn an_ssh_url_without_a_host_is_refused() {
        assert_eq!(parse("ssh:///srv"), Err(UrlError::NoHost));
        assert_eq!(parse("ssh://jw@/srv"), Err(UrlError::NoHost));
    }

    #[test]
    fn a_port_outside_1_to_65535_is_refused() {
        for port in ["0", "65536", "abc", ""] {
            assert_eq!(
                parse(&format!("ssh://nas:{port}/srv")),
                Err(UrlError::Port { text: port.into() }),
                "{port:?}"
            );
        }
    }

    #[test]
    fn a_user_or_host_with_other_characters_is_refused() {
        assert_eq!(
            parse("ssh://j w@nas/srv"),
            Err(UrlError::Name { text: "j w".into() })
        );
        assert_eq!(
            parse("ssh://na$s/srv"),
            Err(UrlError::Name {
                text: "na$s".into()
            })
        );
    }

    #[test]
    fn the_ssh_path_follows_the_root_rules() {
        assert_eq!(
            parse("ssh://nas/srv/../x"),
            Err(UrlError::DotComponent {
                component: "..".into()
            })
        );
        assert_eq!(parse("ssh://nas/srv/"), Err(UrlError::TrailingSlash));
        assert_eq!(parse("ssh://nas/"), Err(UrlError::BareRoot));
        assert_eq!(
            parse("ssh://nas/a b"),
            Err(UrlError::Character { found: ' ' })
        );
    }
}

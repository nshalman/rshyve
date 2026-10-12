// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where a stream server listens: `host:port`, or `unix:/path`.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    Tcp(SocketAddr),
    /// Bound 0600 by [`crate::unixsock::bind_restricted`].
    Unix(PathBuf),
}

impl fmt::Display for Listen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(addr) => write!(f, "{addr}"),
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

impl FromStr for Listen {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.strip_prefix("unix:") {
            Some("") => Err("unix: needs a path".into()),
            Some(path) => Ok(Self::Unix(PathBuf::from(path))),
            None => s.parse().map(Self::Tcp).map_err(|_| {
                format!("{s} is neither unix:/path nor an address:port")
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_forms_parse_and_print_back() {
        for text in ["127.0.0.1:3389", "unix:/tmp/vm.rdp"] {
            let listen: Listen = text.parse().expect(text);
            assert_eq!(listen.to_string(), text);
        }
        assert_eq!(
            "unix:/tmp/vm.rdp".parse(),
            Ok(Listen::Unix(PathBuf::from("/tmp/vm.rdp")))
        );
    }

    #[test]
    fn an_empty_path_or_a_bad_address_is_refused() {
        assert!("unix:".parse::<Listen>().is_err());
        assert!("not-an-address".parse::<Listen>().is_err());
    }
}

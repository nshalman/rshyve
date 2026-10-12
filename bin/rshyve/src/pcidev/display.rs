// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a display spec (`fbuf`) asks for.

use std::path::PathBuf;

use anyhow::Context;
use slog::Logger;

/// What an `fbuf` spec asks for.
#[derive(Debug)]
pub(super) struct FbufConfig {
    pub(super) vnc_path: PathBuf,
    pub(super) vnc_password: Option<String>,
    pub(super) width: u16,
    pub(super) height: u16,
}

/// Parse `vga=off,unix=/path[,password=pw][,password-file=/path][,w=N][,h=N]`.
///
/// A key that does not parse is refused rather than replaced by the
/// default: `w=abc` used to boot a 1024x768 framebuffer and say nothing,
/// so an operator's typo looked like the VMM ignoring them.
pub(super) fn parse_fbuf_config(
    config: &str,
    vm_name: &str,
    log: &Logger,
) -> anyhow::Result<FbufConfig> {
    let mut parsed = FbufConfig {
        vnc_path: PathBuf::from("/var/run/rshyve")
            .join(vm_name)
            .join("vnc.sock"),
        vnc_password: None,
        width: 1024,
        height: 768,
    };
    let mut password_in_argv = false;

    for opt in config.split(',') {
        if let Some(p) = opt.strip_prefix("unix=") {
            parsed.vnc_path = PathBuf::from(p);
        } else if let Some(p) = opt.strip_prefix("password-file=") {
            parsed.vnc_password =
                Some(crate::secret::read_file(std::path::Path::new(p), log)?);
        } else if let Some(p) = opt.strip_prefix("password=") {
            parsed.vnc_password = Some(p.to_string());
            password_in_argv = true;
        } else if let Some(w) = opt.strip_prefix("w=") {
            parsed.width = parse_fbuf_dimension("w", w)?;
        } else if let Some(h) = opt.strip_prefix("h=") {
            parsed.height = parse_fbuf_dimension("h", h)?;
        } else if !opt.is_empty() && opt != "vga=off" {
            anyhow::bail!("fbuf: '{opt}' is not an fbuf option");
        }
    }

    if password_in_argv {
        crate::secret::warn_argv_exposure(
            log,
            "fbuf password=",
            "fbuf password-file=",
        );
    }
    Ok(parsed)
}

fn parse_fbuf_dimension(key: &str, value: &str) -> anyhow::Result<u16> {
    let parsed: u16 = value
        .parse()
        .with_context(|| format!("fbuf: {key}={value} is not 1..=65535"))?;
    if parsed == 0 {
        anyhow::bail!("fbuf: {key}=0 describes no framebuffer");
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn null_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn an_fbuf_size_that_does_not_parse_is_refused() {
        // Silently booting 1024x768 made a typo look like the VMM
        // ignoring the operator.
        for spec in ["w=abc", "h=abc", "w=100000", "w=0", "h=0", "w="] {
            let error = parse_fbuf_config(spec, "vm", &null_log())
                .expect_err("a size that does not parse must be refused");
            assert!(format!("{error:#}").contains("fbuf"), "{spec}: {error:#}");
        }
    }

    #[test]
    fn an_fbuf_spec_is_parsed_whole() {
        let parsed = parse_fbuf_config(
            "vga=off,unix=/tmp/vnc.sock,password=pw,w=800,h=600",
            "vm",
            &null_log(),
        )
        .expect("a complete spec");

        assert_eq!(parsed.vnc_path, PathBuf::from("/tmp/vnc.sock"));
        assert_eq!(parsed.vnc_password.as_deref(), Some("pw"));
        assert_eq!((parsed.width, parsed.height), (800, 600));
    }

    #[test]
    fn an_fbuf_spec_defaults_to_the_vm_socket_path() {
        let parsed =
            parse_fbuf_config("", "testvm", &null_log()).expect("no options");

        assert_eq!(
            parsed.vnc_path,
            PathBuf::from("/var/run/rshyve/testvm/vnc.sock"),
        );
        assert_eq!((parsed.width, parsed.height), (1024, 768));
    }

    #[test]
    fn an_unknown_fbuf_option_is_refused() {
        let error = parse_fbuf_config("widht=800", "vm", &null_log())
            .expect_err("a misspelled key");

        assert!(format!("{error:#}").contains("widht=800"), "{error:#}");
    }
}

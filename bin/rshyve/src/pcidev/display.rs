// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a display spec (`fbuf`, `virtio-gpu`) asks for, and the RDP
//! server a `virtio-gpu` spec can start.

use std::path::PathBuf;
#[cfg(feature = "rdp")]
use std::sync::Arc;

use anyhow::Context;
use slog::Logger;
use vmm_core::listen::Listen;

/// Serve `display` over RDP when its spec asked for it, and bind the
/// server as the VM's audio sink. Returns how to stop it.
#[cfg(feature = "rdp")]
pub(super) fn start_rdp(
    rdp: Option<RdpOptions>,
    display: Arc<dyn vmm_devices::display::DisplaySource>,
    ctx: &vmm_machine::PciDeviceCtx<'_>,
    log: &Logger,
) -> anyhow::Result<Option<vmm_virtio::gpu::HaltHook>> {
    let Some(r) = rdp else {
        return Ok(None);
    };
    let server = vmm_rdp::RdpServer::start(
        "gpu-rdp",
        display,
        Arc::clone(ctx.input),
        ctx.audio,
        vmm_rdp::RdpConfig {
            addr: r.addr,
            cert: r.cert,
            key: r.key,
            username: r.username,
            password: r.password,
        },
        log.new(slog::o!("component" => "virtio-gpu-rdp")),
    )?;
    Ok(Some(Box::new(move || server.shutdown())))
}

/// Without the `rdp` feature there is no server to start:
/// [`parse_display_config`] has already refused a spec that asks for one.
#[cfg(not(feature = "rdp"))]
pub(super) fn start_rdp(
    rdp: Option<RdpOptions>,
    _display: std::sync::Arc<dyn vmm_devices::display::DisplaySource>,
    _ctx: &vmm_machine::PciDeviceCtx<'_>,
    _log: &Logger,
) -> anyhow::Result<Option<vmm_virtio::gpu::HaltHook>> {
    debug_assert!(rdp.is_none(), "an RDP spec passed the parser");
    Ok(None)
}

/// The VNC socket a virtio-gpu serves when its spec names none. Not
/// fbuf's, so a VM can have both.
pub(super) const GPU_VNC_SOCKET: &str = "gpu-vnc.sock";

/// What a display spec (`fbuf`, `virtio-gpu`) asks for.
#[derive(Debug)]
pub(super) struct DisplayConfig {
    pub(super) vnc_path: PathBuf,
    pub(super) vnc_password: Option<String>,
    pub(super) width: u16,
    pub(super) height: u16,
    /// RDP as well as VNC, when the spec asks for it.
    pub(super) rdp: Option<RdpOptions>,
}

/// How a display serves RDP. Every field is required: the server is not
/// run without TLS or without a password.
#[cfg_attr(not(feature = "rdp"), allow(dead_code))]
pub(super) struct RdpOptions {
    pub(super) addr: Listen,
    pub(super) cert: PathBuf,
    pub(super) key: PathBuf,
    pub(super) username: String,
    pub(super) password: String,
}

impl std::fmt::Debug for RdpOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RdpOptions")
            .field("addr", &self.addr)
            .field("cert", &self.cert)
            .field("key", &self.key)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Parse `vga=off,unix=/path[,password=pw][,password-file=/path][,w=N][,h=N]`
/// for `driver`, and the `rdp=`, `rdp-cert=`, `rdp-key=`, `rdp-user=`
/// and `rdp-password-file=` keys, which only virtio-gpu takes. Without
/// `unix=` the socket is `socket` in the VM's run directory.
///
/// A key that does not parse is refused rather than replaced by the
/// default, so an operator's typo is not mistaken for the VMM ignoring
/// them.
pub(super) fn parse_display_config(
    driver: &str,
    socket: &str,
    config: &str,
    vm_name: &str,
    log: &Logger,
) -> anyhow::Result<DisplayConfig> {
    let mut parsed = DisplayConfig {
        vnc_path: PathBuf::from("/var/run/rshyve").join(vm_name).join(socket),
        vnc_password: None,
        width: 1024,
        height: 768,
        rdp: None,
    };
    let mut password_in_argv = false;
    let mut rdp_addr = None;
    let mut rdp_cert = None;
    let mut rdp_key = None;
    let mut rdp_user = None;
    let mut rdp_password = None;

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
            parsed.width = parse_display_dimension(driver, "w", w)?;
        } else if let Some(h) = opt.strip_prefix("h=") {
            parsed.height = parse_display_dimension(driver, "h", h)?;
        } else if let Some(a) = opt.strip_prefix("rdp=") {
            rdp_addr =
                Some(a.parse::<Listen>().map_err(|why| {
                    anyhow::anyhow!("{driver}: rdp={a}: {why}")
                })?);
        } else if let Some(p) = opt.strip_prefix("rdp-cert=") {
            rdp_cert = Some(PathBuf::from(p));
        } else if let Some(p) = opt.strip_prefix("rdp-key=") {
            rdp_key = Some(PathBuf::from(p));
        } else if let Some(u) = opt.strip_prefix("rdp-user=") {
            rdp_user = Some(u.to_string());
        } else if let Some(p) = opt.strip_prefix("rdp-password-file=") {
            rdp_password =
                Some(crate::secret::read_file(std::path::Path::new(p), log)?);
        } else if !opt.is_empty() && opt != "vga=off" {
            anyhow::bail!("{driver}: '{opt}' is not an {driver} option");
        }
    }

    if password_in_argv {
        crate::secret::warn_argv_exposure(
            log,
            &format!("{driver} password="),
            &format!("{driver} password-file="),
        );
    }

    let rdp_asked = rdp_addr.is_some()
        || rdp_cert.is_some()
        || rdp_key.is_some()
        || rdp_user.is_some()
        || rdp_password.is_some();
    if rdp_asked {
        if driver != "virtio-gpu" {
            anyhow::bail!("{driver}: RDP is served only for virtio-gpu");
        }
        match (rdp_addr, rdp_cert, rdp_key, rdp_user, rdp_password) {
            (
                Some(addr),
                Some(cert),
                Some(key),
                Some(username),
                Some(password),
            ) => {
                // Either empty lets anyone in.
                if username.is_empty() || password.is_empty() {
                    anyhow::bail!(
                        "{driver}: RDP needs a user and a password, and \
                         rdp-user or rdp-password-file is empty"
                    );
                }
                if !cfg!(feature = "rdp") {
                    anyhow::bail!(
                        "{driver}: RDP was asked for, and this rshyve was \
                         built without its `rdp` feature"
                    );
                }
                parsed.rdp = Some(RdpOptions {
                    addr,
                    cert,
                    key,
                    username,
                    password,
                });
            }
            (addr, cert, key, user, password) => {
                let missing: Vec<&str> = [
                    ("rdp", addr.is_none()),
                    ("rdp-cert", cert.is_none()),
                    ("rdp-key", key.is_none()),
                    ("rdp-user", user.is_none()),
                    ("rdp-password-file", password.is_none()),
                ]
                .iter()
                .filter(|(_, missing)| *missing)
                .map(|(name, _)| *name)
                .collect();
                anyhow::bail!(
                    "{driver}: RDP needs TLS and a password; missing {}",
                    missing.join(", ")
                );
            }
        }
    }
    Ok(parsed)
}

fn parse_display_dimension(
    driver: &str,
    key: &str,
    value: &str,
) -> anyhow::Result<u16> {
    let parsed: u16 = value
        .parse()
        .with_context(|| format!("{driver}: {key}={value} is not 1..=65535"))?;
    if parsed == 0 {
        anyhow::bail!("{driver}: {key}=0 describes no framebuffer");
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn null_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn parse_fbuf_config(
        config: &str,
        vm_name: &str,
    ) -> anyhow::Result<DisplayConfig> {
        parse_display_config("fbuf", "vnc.sock", config, vm_name, &null_log())
    }

    #[test]
    fn an_fbuf_size_that_does_not_parse_is_refused() {
        // Silently booting 1024x768 made a typo look like the VMM
        // ignoring the operator.
        for spec in ["w=abc", "h=abc", "w=100000", "w=0", "h=0", "w="] {
            let error = parse_fbuf_config(spec, "vm")
                .expect_err("a size that does not parse must be refused");
            assert!(format!("{error:#}").contains("fbuf"), "{spec}: {error:#}");
        }
    }

    #[test]
    fn an_fbuf_spec_is_parsed_whole() {
        let parsed = parse_fbuf_config(
            "vga=off,unix=/tmp/vnc.sock,password=pw,w=800,h=600",
            "vm",
        )
        .expect("a complete spec");

        assert_eq!(parsed.vnc_path, PathBuf::from("/tmp/vnc.sock"));
        assert_eq!(parsed.vnc_password.as_deref(), Some("pw"));
        assert_eq!((parsed.width, parsed.height), (800, 600));
    }

    #[test]
    fn an_fbuf_spec_defaults_to_the_vm_socket_path() {
        let parsed = parse_fbuf_config("", "testvm").expect("no options");

        assert_eq!(
            parsed.vnc_path,
            PathBuf::from("/var/run/rshyve/testvm/vnc.sock"),
        );
        assert_eq!((parsed.width, parsed.height), (1024, 768));
    }

    #[test]
    fn an_unknown_fbuf_option_is_refused() {
        let error =
            parse_fbuf_config("widht=800", "vm").expect_err("a misspelled key");

        assert!(format!("{error:#}").contains("widht=800"), "{error:#}");
    }

    fn gpu_config(config: &str) -> anyhow::Result<DisplayConfig> {
        parse_display_config(
            "virtio-gpu",
            GPU_VNC_SOCKET,
            config,
            "testvm",
            &null_log(),
        )
    }

    #[cfg(feature = "rdp")]
    #[test]
    fn a_virtio_gpu_spec_can_serve_rdp() {
        let dir = tempfile::tempdir().expect("temp dir");
        let pw = dir.path().join("rdp.pw");
        std::fs::write(&pw, "s3cret\n").expect("write password");
        let spec = format!(
            "rdp=127.0.0.1:3389,rdp-cert=/c.pem,rdp-key=/k.pem,\
             rdp-user=console,rdp-password-file={}",
            pw.display()
        );
        let rdp = gpu_config(&spec)
            .expect("a full RDP spec")
            .rdp
            .expect("rdp");
        assert_eq!(rdp.addr, Listen::Tcp("127.0.0.1:3389".parse().unwrap()));
        assert_eq!(rdp.cert, PathBuf::from("/c.pem"));
        assert_eq!(rdp.key, PathBuf::from("/k.pem"));
        assert_eq!(rdp.username, "console");
        assert_eq!(rdp.password, "s3cret");
    }

    #[cfg(feature = "rdp")]
    #[test]
    fn a_virtio_gpu_spec_can_serve_rdp_on_a_unix_socket() {
        let dir = tempfile::tempdir().expect("temp dir");
        let pw = dir.path().join("rdp.pw");
        std::fs::write(&pw, "s3cret\n").expect("write password");
        let spec = format!(
            "rdp=unix:/tmp/vm.rdp,rdp-cert=/c.pem,rdp-key=/k.pem,\
             rdp-user=console,rdp-password-file={}",
            pw.display()
        );
        let rdp = gpu_config(&spec)
            .expect("a full RDP spec")
            .rdp
            .expect("rdp");
        assert_eq!(rdp.addr, Listen::Unix(PathBuf::from("/tmp/vm.rdp")));
        assert!(gpu_config("rdp=unix:").is_err(), "a socket needs a path");
    }

    /// A build without RDP refuses a spec that asks for it, rather than
    /// booting a VM whose remote display is silently missing.
    #[cfg(not(feature = "rdp"))]
    #[test]
    fn a_build_without_rdp_refuses_an_rdp_spec() {
        let dir = tempfile::tempdir().expect("temp dir");
        let pw = dir.path().join("rdp.pw");
        std::fs::write(&pw, "s3cret\n").expect("write password");
        let spec = format!(
            "rdp=127.0.0.1:3389,rdp-cert=/c.pem,rdp-key=/k.pem,\
             rdp-user=console,rdp-password-file={}",
            pw.display()
        );
        let error = gpu_config(&spec).expect_err("built without RDP");
        assert!(format!("{error:#}").contains("`rdp` feature"), "{error:#}");
    }

    /// An empty password, or an empty user, would let anyone in: a
    /// truncated or template password file is refused, not served.
    #[test]
    fn an_empty_rdp_password_or_user_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let empty = dir.path().join("empty.pw");
        std::fs::write(&empty, "\n").expect("write password");
        let good = dir.path().join("rdp.pw");
        std::fs::write(&good, "s3cret\n").expect("write password");
        for (user, pw) in [("console", &empty), ("", &good)] {
            let spec = format!(
                "rdp=127.0.0.1:3389,rdp-cert=/c.pem,rdp-key=/k.pem,\
                 rdp-user={user},rdp-password-file={}",
                pw.display()
            );
            let error = gpu_config(&spec).expect_err("an empty credential");
            assert!(format!("{error:#}").contains("empty"), "{error:#}");
        }
    }

    #[test]
    fn a_display_without_rdp_serves_none() {
        assert!(gpu_config("w=800,h=600").expect("spec").rdp.is_none());
    }

    /// RDP is on TCP: it is refused rather than served without TLS or
    /// without a password.
    #[test]
    fn an_incomplete_rdp_spec_is_refused() {
        let error = gpu_config("rdp=127.0.0.1:3389,rdp-cert=/c.pem")
            .expect_err("no key, user or password");
        let message = format!("{error:#}");
        for missing in ["rdp-key", "rdp-user", "rdp-password-file"] {
            assert!(message.contains(missing), "{message}");
        }
        assert!(gpu_config("rdp=not-an-address").is_err());
        assert!(
            gpu_config("rdp-password=pw").is_err(),
            "no password in argv"
        );
    }

    #[test]
    fn fbuf_does_not_serve_rdp() {
        let error = parse_fbuf_config("rdp=127.0.0.1:3389", "vm")
            .expect_err("fbuf has no RDP");
        assert!(format!("{error:#}").contains("virtio-gpu"), "{error:#}");
    }

    #[test]
    fn a_virtio_gpu_spec_has_its_own_default_socket() {
        let parsed = parse_display_config(
            "virtio-gpu",
            GPU_VNC_SOCKET,
            "w=1280,h=800",
            "testvm",
            &null_log(),
        )
        .expect("a gpu spec");
        // Not fbuf's vnc.sock: both devices may be present.
        assert_eq!(
            parsed.vnc_path,
            PathBuf::from("/var/run/rshyve/testvm/gpu-vnc.sock"),
        );
        assert_eq!((parsed.width, parsed.height), (1280, 800));
    }
}

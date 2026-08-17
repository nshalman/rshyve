use std::process::Command;

fn find_git() -> &'static str {
    for path in ["/opt/tools/bin/git", "/usr/bin/git", "git"] {
        if Command::new(path).arg("--version").output().is_ok() {
            return match path {
                "/opt/tools/bin/git" => "/opt/tools/bin/git",
                "/usr/bin/git" => "/usr/bin/git",
                _ => "git",
            };
        }
    }
    "git"
}

fn main() {
    let git = find_git();

    // Embed git commit hash at compile time
    let output = Command::new(git)
        .args(["rev-parse", "--short", "HEAD"])
        .output();

    let commit = match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => "unknown".to_string(),
    };

    println!("cargo:rustc-env=VMM_GIT_COMMIT={commit}");

    // Rerun if git HEAD changes
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/");

    // Embed a runpath for OpenSSL so the resulting `vmm` binary
    // loads `libcrypto.so.3` from the SmartOS pkgsrc location
    // (typically `/opt/tools/lib`) without `LD_LIBRARY_PATH` set.
    // We discover the directory ourselves rather than relying on
    // build-script metadata propagation: `cargo:KEY=VAL` from a
    // sibling lib's build script is exposed as `DEP_<LINK>_<KEY>`
    // env, but only when the consumer's build.rs runs *after* the
    // producer's, and we don't want to take a hard order dependency.
    let pkgconfig =
        std::env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".into());
    if let Ok(out) = Command::new(&pkgconfig)
        .args(["--libs-only-L", "openssl"])
        .output()
    {
        if out.status.success() {
            for arg in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                if let Some(dir) = arg.strip_prefix("-L") {
                    println!("cargo:rustc-link-arg=-Wl,-R,{}", dir);
                }
            }
        }
    }
}

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

    // No runpath is emitted here on purpose. The pkgsrc gcc driver
    // already injects /opt/local/lib into every binary it links, and
    // vmm-tpm-sys resolves OpenSSL from the platform image, so an
    // extra -Wl,-R only duplicated an entry that was already there.
}

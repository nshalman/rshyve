// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fhrun`: make a Linux binary feel like a process by wrapping it in a
//! microVM.

mod cpio;
mod initramfs;
mod launcher;
mod manifest;

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::manifest::Manifest;

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("fhrun: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<i32> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        usage();
        return Ok(2);
    }

    match args[1].as_str() {
        "-h" | "--help" => {
            usage();
            Ok(0)
        }
        "--check" => {
            let path = args.get(2).context("--check needs a manifest path")?;
            let m = Manifest::load(Path::new(path))?;
            println!("ok: {} ({} vCPU, {})", m.name, m.vcpus, m.memory);
            Ok(0)
        }
        "--print-argv" => {
            let path =
                args.get(2).context("--print-argv needs a manifest path")?;
            let m = Manifest::load(Path::new(path))?;
            // Placeholder paths: this prints the shape of the command
            // line without building an initramfs or a temp directory.
            let inv = launcher::build_vmm_invocation(
                &m,
                Path::new("<initramfs>"),
                Path::new("<runtime-dir>"),
            )?;
            println!("{}", inv.program.display());
            for a in &inv.args {
                println!("{}", a.to_string_lossy());
            }
            Ok(0)
        }
        "--emit-initramfs" => {
            let manifest_path = args.get(2).context("missing manifest path")?;
            let out_path = args.get(3).context("missing output path")?;
            let m = Manifest::load(Path::new(manifest_path))?;
            initramfs::build_to_path(&m, Path::new(out_path))?;
            println!("wrote {out_path}");
            Ok(0)
        }
        path if !path.starts_with('-') => {
            let m = Manifest::load(Path::new(path))?;
            let outcome = launcher::launch(&m)?;
            // A VMM killed by a signal has no exit code. Reporting 0
            // there would be a lie, so report 1.
            Ok(outcome.exit_code.unwrap_or(1))
        }
        other => {
            eprintln!("fhrun: unknown flag '{other}'");
            usage();
            Ok(2)
        }
    }
}

fn usage() {
    eprintln!(
        "Usage:\n  \
         fhrun <manifest.json>\n  \
         fhrun --check <manifest.json>\n  \
         fhrun --print-argv <manifest.json>\n  \
         fhrun --emit-initramfs <manifest.json> <out.cpio>\n"
    );
}

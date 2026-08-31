# libtpms vendoring notes

Source: https://github.com/stefanberger/libtpms
Revision: `521c51073fe6f7c56023db78e56961fcaf7906e8` (untagged `master`, 2026-03-13)

There is no upstream `v0.11.0` tag; 0.11.0 is only the in-development
version string in `configure.ac`. This file previously recorded that
string as the source revision. The commit above is authoritative and was
recovered by diffing the vendored tree against upstream archives.

License: BSD-3-Clause (see LICENSE in this directory). Note that
`m4/ax_check_linker_flag.m4` is GPL-2.0-or-later with the Autoconf Macro
exception; see `THIRD_PARTY.md`.

This revision predates the fixes for CVE-2026-6726 and CVE-2026-6727,
both of which are present in this tree. See `THIRD_PARTY.md` and
`docs/security/open-findings.md`.

## Patches applied for illumos

`patches/` is applied to the `OUT_DIR` copy by
`crates/vmm-tpm-sys/build.rs`, which fails the build if a patch does not
apply. The submodule stays pristine, so these are, and remain, the only
changes to the upstream tree:

1. **`src/tpm2/TPMCmd/TpmConfiguration/TpmConfiguration/TpmProfile_Common.h`**
   Added `__sun` / `__illumos__` block to the OS endian-detection cascade.
   Without this, the file errors out with `#error Unsupported OS`.

2. **`src/tpm2/RuntimeProfile.c`**
   Replaced 3 calls to BSD `index(3)` with POSIX `strchr(3)`. The illumos
   default header set does not expose `index()` without obscure feature
   macros; `strchr()` is the modern POSIX equivalent and behaves identically.

## Configure flags required for illumos

Set in the cargo `build.rs` (do not edit configure.ac):

```
CFLAGS="-D_POSIX_C_SOURCE=200809L -D_XOPEN_SOURCE=700 -D__EXTENSIONS__"
./autogen.sh --with-openssl --with-tpm2 --disable-hardening
```

* `_POSIX_C_SOURCE=200809L` exposes `dprintf`/`vdprintf`.
* `__EXTENSIONS__` keeps illumos compatibility extensions visible.
* `--disable-hardening` strips the `-Wl,-z,now` and other Linux-specific
  hardening flags that gcc on illumos rejects.

## Updating to a newer libtpms

Move the submodule to the new revision, re-run a build, and update the
revision recorded above and in `THIRD_PARTY.md`. The build applies
`patches/*.patch` and fails loudly if one no longer applies, so a
refresh cannot silently drop an illumos fix. The patches are
deliberately minimal so they should rebase cleanly across versions.

`src/tpm2/NVMarshal.c` is the vTPM state serializer, and vTPM state is
the root of a BitLocker-protected guest's disk encryption. A refresh
that changes the state format locks such guests out. Any update must be
validated by resuming an existing vTPM state blob, not only by booting a
fresh guest.

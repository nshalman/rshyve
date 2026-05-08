# libtpms vendoring notes

Source: https://github.com/stefanberger/libtpms (v0.11.0)

License: BSD (see LICENSE in this directory).

## Patches applied for illumos

These are the only changes to the upstream tree:

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

Re-fetch upstream, re-apply the two patches above, re-run a build,
update the version string in this file. The patches are deliberately
minimal so they should rebase cleanly across upstream versions.

#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#

#
# Copyright 2026 Edgecast Cloud LLC.
#

#
# This Makefile is only used when rust-bhyve is pulled into a full build
# of smartos-live in its projects/local/rust-bhyve directory. Ordinary
# development uses cargo directly and does not need it.
#
# The include paths below reach the top-level smartos-live deps/eng, so
# this file only works from projects/local/rust-bhyve.
#

# Makefile.rust.defs reads the channel from rust-toolchain.toml and
# fetches its own toolchain into $(RUST_TOOLCHAIN_DIR).
RUST_TOOLCHAIN_DIR=rust
RUST_USE_BOOTSTRAP=false

TAR=tar
GIT=git
INSTALL=/usr/sbin/install
RMTREE=rm -rf

# Downloaded rustup does not find the platform CA bundle on its own.
export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

include ../../../deps/eng/tools/mk/Makefile.defs
include ../../../deps/eng/tools/mk/Makefile.rust.defs
include ../../../deps/eng/tools/mk/Makefile.rust.targ

.PHONY: all world test update install manifest

# Only the two VMM binaries are platform content. fhrun and
# vmm-migrate-agent are development and lab tooling, and fhrun-init is a
# Linux musl binary in an excluded workspace, so none of them build here.
all: rust-fetch
	(. rust/cargo/env && $(CARGO) build --release -p rshyve -p firehyve)

world: all

test: all
	(. rust/cargo/env && $(CARGO) test --release -p rshyve -p firehyve)

update:
	$(GIT) pull --rebase

manifest:
	cp manifest $(DESTDIR)/$(DESTNAME)

install: world manifest
	@mkdir -p $(DESTDIR)/usr/sbin
	@cp target/release/rshyve $(DESTDIR)/usr/sbin/.
	@cp target/release/firehyve $(DESTDIR)/usr/sbin/.

clean::
	$(RMTREE) target

# COPR invokes this as: make -f .copr/Makefile srpm outdir=<dir>
#
# IMPORTANT: COPR runs this inside MOCK, not a plain container, and mock redefines rpm's
# %{_topdir} to /builddir/build. A version that calls `rpmdev-setuptree` and then copies into
# $(HOME)/rpmbuild/SPECS -- which is where the tree lands in a plain container and is NOT
# where it lands under mock -- fails with 'No such file or directory'. Verifying such a
# recipe in a container therefore proves nothing about COPR. This version does not depend on
# %{_topdir}, $(HOME), or a source tree existing at all: it points _sourcedir and _srcrpmdir
# explicitly, so the environment cannot move them.
#
# WHAT THIS SUPPLIES, AND WHY
# ---------------------------
# packaging/copr/etr.spec is a TEMPLATE: its Version: is `@VERSION@` and its Source0 is a
# local tarball. This recipe supplies both, from the checkout COPR cloned:
#
#   version  <- Cargo.toml, via scripts/render_packaging.py
#   Source0  <- an archive of this checkout
#
# Neither needs a published tag or a network fetch, which is what lets a PR build its own
# SRPM -- impossible while Source0 pinned a tag tarball that does not exist until after a
# release, so CI could only ever build the PREVIOUS release's source.
#
# HOW THE SOURCE ARCHIVE IS BUILT, AND THE TRAP THAT DECIDED IT
# -------------------------------------------------------------
# `git archive` when there is a repository to ask, plain `tar` otherwise. The fallback is not
# cosmetic: whether COPR's mock buildroot carries `.git` alongside the checkout is not
# something this repo can verify without a COPR build, so the recipe works either way and
# PRINTS which path it took -- the first COPR build after this lands answers it from its log.
#
# The reason the fallback needs explicit excludes is a trap measured in the sibling repo:
# **GNU tar's `--exclude-vcs-ignores` does not implement .gitignore semantics.** Given a
# .gitignore like this one it honours bare filenames (`WIP.md`) and silently keeps both the
# directory form and the anchored form (`/target`) -- so a naive recipe packs the private
# cross-machine handoff log into the SRPM, where it would reach the published -debugsource
# package. The excludes below are also written WITHOUT a leading `./`, which is the second
# half of the same trap: `--exclude=./target` anchors at the top level and silently keeps
# `tools/stress/target/`.
#
# An SRPM is published and cannot be recalled, which is why the guard below is a hard failure
# rather than a warning -- and why it also asserts the files whose ABSENCE would be just as
# bad: no Cargo.lock means unpinned resolution, no LICENSE means a GPL obligation shipped
# unmet, and no man/*.1 means the %files section fails deep inside rpmbuild instead of here.

SPEC_IN := packaging/copr/etr.spec
BUILD   := $(CURDIR)/.copr-build
RENDER  := scripts/render_packaging.py

.PHONY: srpm

srpm:
	# python3 is present in every Fedora buildroot (rpm itself depends on it) and git usually
	# is not. Both are installed explicitly rather than assumed -- a build that fails for a
	# missing tool should say so at its first step, not halfway through.
	dnf -y install rpm-build python3 git
	rm -rf $(BUILD)
	mkdir -p $(BUILD) $(outdir)
	V=$$(python3 $(RENDER) --print-version); \
	echo "version from Cargo.toml: $$V"; \
	python3 $(RENDER) --target copr --version "$$V" --out $(BUILD)/etr.spec; \
	TARBALL=$(BUILD)/etr-$$V.tar.gz; \
	if git rev-parse --git-dir >/dev/null 2>&1; then \
	    echo "source: git archive HEAD (the committed tree)"; \
	    git archive --format=tar.gz --prefix="etr-$$V/" -o "$$TARBALL" HEAD; \
	else \
	    echo "source: tar of the working tree (no git repository here)"; \
	    tar --exclude=.git --exclude=target --exclude=.copr-build \
	        --exclude=WIP.md --exclude=.claude --exclude=etrs_fwd.json.gz \
	        --exclude=__pycache__ --exclude='*.pyc' \
	        --transform "s,^[.],etr-$$V," \
	        -czf "$$TARBALL" .; \
	fi; \
	tar -tzf "$$TARBALL" > $(BUILD)/listing.txt; \
	echo "packed $$(wc -l < $(BUILD)/listing.txt) entries as etr-$$V/"; \
	if grep -qE "^etr-[^/]+/(WIP[.]md|target/|tools/stress/target/|[.]git/|[.]claude/|etrs_fwd[.]json[.]gz)" $(BUILD)/listing.txt; then \
	    echo "error: the source archive contains files that must never ship:" >&2; \
	    grep -E "^etr-[^/]+/(WIP[.]md|target/|tools/stress/target/|[.]git/|[.]claude/|etrs_fwd[.]json[.]gz)" $(BUILD)/listing.txt >&2; \
	    exit 1; \
	fi; \
	for f in Cargo.toml Cargo.lock LICENSE README.md man/etr.1 man/etrs.1; do \
	    grep -qx "etr-$$V/$$f" $(BUILD)/listing.txt \
	        || { echo "error: the source archive is missing $$f" >&2; exit 1; }; \
	done; \
	echo "source archive verified"
	rpmbuild -bs --define '_sourcedir $(BUILD)' --define '_srcrpmdir $(outdir)' $(BUILD)/etr.spec

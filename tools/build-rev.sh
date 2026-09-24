# shellcheck shell=bash
#
# The source revision a build stamps into a VMM binary.
#
# tools/remote-build.sh sources this and tools/build-rev-test.sh tests
# it. The build host holds no clone, because the sync excludes .git, so
# the driver computes the value and passes it in TRITON_BUILD_REV.

# The marker for a tree holding changes that no commit holds.
DIRTY_SUFFIX="-dirty"

# Print the revision of the tree at $1, or nothing.
build_rev() {
    local src="$1" rev changes
    # git walks up to a parent repository when a directory holds no
    # .git of its own, and this tree sits inside other checkouts. A
    # commit from the repository above looks right and names code that
    # was never built, which is the failure this stamp exists to end.
    [ "$(git -C "$src" rev-parse --show-toplevel 2>/dev/null)" = "$src" ] ||
        return 0
    rev="$(git -C "$src" rev-parse --short HEAD 2>/dev/null)" || return 0
    # rsync sends the working tree and not a commit, so untracked files
    # and submodule changes reach the build host. status.showUntrackedFiles
    # and diff.ignoreSubmodules can hide both, so the flags decide what
    # counts as dirty and not the local configuration. A status that
    # cannot be read counts as changed.
    if ! changes="$(git -C "$src" status --porcelain \
        --untracked-files=normal --ignore-submodules=none 2>/dev/null)" ||
        [ -n "$changes" ]; then
        rev="$rev$DIRTY_SUFFIX"
    fi
    printf '%s' "$rev"
}

# Is $1 an abbreviated or full object name, with an optional dirty
# marker?
#
# The value is spliced into a command a remote root shell parses, so
# the test covers the whole value: `grep` answers for a line, and a
# multi-line value whose first line is hex would pass it.
is_object_name() {
    local core="${1%"$DIRTY_SUFFIX"}"
    case "$core" in
        '' | *[!0-9a-f]*) return 1 ;;
    esac
    # git abbreviates to 7 digits and lengthens the name when 7 is
    # ambiguous. 40 is a full SHA-1 name.
    [ "${#core}" -ge 7 ] && [ "${#core}" -le 40 ]
}

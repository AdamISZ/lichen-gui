#!/usr/bin/env bash
# Build lichen-gui and lichen-devstack (target/release/) against the lichen
# this release names (LICHEN_TAG) and the ln-gap that lichen pins (its
# LNGAP_REV), both checked out beside this directory, as Cargo.toml's
# paths expect:
#
#   ../lichen   cloned at LICHEN_TAG if it isn't there
#   ../ln-gap   cloned at lichen's LNGAP_REV if it isn't there
#
# A checkout already there is checked, never moved: at another revision,
# or with changes to its tracked files, the build stops and says what to
# do. LICHEN_GUI_DEV=1 builds against whatever is there (developing
# lichen and the GUI together).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
parent=$(dirname "$here")
lichen="$parent/lichen"
lngap="$parent/ln-gap"

die() { echo "build.sh: $*" >&2; exit 1; }
command -v git >/dev/null || die "git is needed"
command -v cargo >/dev/null || die "cargo is needed: install Rust from https://rustup.rs (rust-toolchain.toml picks the version)"

tag=$(tr -d '[:space:]' < "$here/LICHEN_TAG")
[ -n "$tag" ] || die "LICHEN_TAG is empty"

# checkout DIR URL REV NAME: DIR cloned at REV if it isn't there, else
# checked to be at REV with its tracked files unchanged
checkout() {
    local dir=$1 url=$2 rev=$3 name=$4
    if [ ! -e "$dir" ]; then
        echo "cloning $name at $rev into $dir"
        git clone --quiet "$url" "$dir"
        git -C "$dir" -c advice.detachedHead=false checkout --quiet "$rev"
        return
    fi
    # its own checkout, not a directory inside some other repository
    [ "$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null)" = "$(cd "$dir" && pwd -P)" ] \
        || die "$dir is there but isn't a git checkout: move it, or build by hand"
    [ "${LICHEN_GUI_DEV:-}" = 1 ] && { echo "$name: using $dir as it is (LICHEN_GUI_DEV=1)"; return; }
    local want have
    want=$(git -C "$dir" rev-parse --verify --quiet "$rev^{commit}") \
        || die "$dir doesn't have $rev: run 'git -C $dir fetch --tags origin' (or LICHEN_GUI_DEV=1 to build against it as it is)"
    have=$(git -C "$dir" rev-parse HEAD)
    [ "$have" = "$want" ] \
        || die "$dir is at $(git -C "$dir" describe --tags --always HEAD), not $rev: check out $rev there ('git -C $dir checkout $rev'), or set LICHEN_GUI_DEV=1 to build against it as it is"
    [ -z "$(git -C "$dir" status --porcelain --untracked-files=no)" ] \
        || die "$dir has changes to its tracked files: commit or set them aside, or set LICHEN_GUI_DEV=1 to build with them"
    echo "$name: $dir at $rev"
}

checkout "$lichen" https://github.com/AdamISZ/lichen.git "$tag" "lichen"
lngap_rev=$(tr -d '[:space:]' < "$lichen/LNGAP_REV")
checkout "$lngap" https://github.com/AdamISZ/ln-gap.git "$lngap_rev" "ln-gap"

cd "$here"
# the lock names lichen's version: --locked holds it to LICHEN_TAG's
if [ "${LICHEN_GUI_DEV:-}" = 1 ]; then cargo build --release; else cargo build --release --locked; fi
echo "built: $here/target/release/lichen-gui (and lichen-devstack)"

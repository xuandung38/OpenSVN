#!/bin/sh
# C compiler for clippy against aarch64-apple-darwin from Linux: `zig cc` ships the macOS libc
# headers that the bundled sqlite needs, so no macOS SDK is required. Check only, never a build.
#   DOCS_RS=1 CC_aarch64_apple_darwin="$PWD/scripts/dev/zig-cc-macos.sh" AR_aarch64_apple_darwin=llvm-ar \
#     cargo clippy --workspace --all-targets --locked --target aarch64-apple-darwin -- -D warnings
# ZIG: the zig binary (default: zig on PATH; `pip install ziglang` puts one in the ziglang package).
# The target flags cc-rs adds for clang are dropped: zig takes the target from -target.
set -eu
zig="${ZIG:-zig}"
n=$#
while [ "$n" -gt 0 ]; do
  a="$1"; shift; n=$((n - 1))
  case "$a" in
    --target=* | -mmacosx-version-min=* | -mmacos-version-min=*) ;;
    -arch) shift; n=$((n - 1)) ;;
    *) set -- "$@" "$a" ;;
  esac
done
exec "$zig" cc -target aarch64-macos "$@"

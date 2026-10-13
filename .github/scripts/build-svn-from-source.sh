#!/usr/bin/env bash
# Builds Apache Subversion from the official source tarball into a prefix.
#
#   build-svn-from-source.sh <version> <sha512> <prefix>
#
# The tarball is accepted only if its SHA-512 equals both the pinned value passed in and the
# value published next to the release on archive.apache.org/dist. Build dependencies (APR,
# SQLite, serf, lz4, utf8proc, zlib, expat) must already be installed.
set -euo pipefail

version="$1"
pinned_sha512="$2"
prefix="$3"

dist="https://archive.apache.org/dist/subversion"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"

curl --fail --silent --show-error --location --retry 3 -O "$dist/subversion-$version.tar.bz2"
published="$(curl --fail --silent --show-error --location --retry 3 "$dist/subversion-$version.tar.bz2.sha512" | awk '{print $1}')"
actual="$(sha512sum "subversion-$version.tar.bz2" | awk '{print $1}')"

if [ "$published" != "$pinned_sha512" ]; then
  echo "pinned sha512 does not match the one published by the Apache dist for $version" >&2
  echo "  pinned:    $pinned_sha512" >&2
  echo "  published: $published" >&2
  exit 1
fi
if [ "$actual" != "$pinned_sha512" ]; then
  echo "downloaded tarball sha512 mismatch for $version" >&2
  echo "  expected: $pinned_sha512" >&2
  echo "  actual:   $actual" >&2
  exit 1
fi

tar -xjf "subversion-$version.tar.bz2"
cd "subversion-$version"
./configure --prefix="$prefix" \
  --disable-nls --disable-mod-activation \
  --without-apxs --without-berkeley-db --without-swig-perl --without-swig-python --without-swig-ruby \
  --with-serf=/usr --with-lz4=internal --with-utf8proc=internal \
  --with-apr=/usr --with-apr-util=/usr
make -j"$(nproc)"
make install
"$prefix/bin/svn" --version --quiet

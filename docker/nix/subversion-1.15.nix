# svn 1.15.0 built from the official Apache tarball (SHA-512 pinned, same pin as docker/compose.yml).
# Used to generate fixtures and run the svn oracle without Docker:
#   nix-build docker/nix/subversion-1.15.nix -o target/svn-1.15
#   SVN=target/svn-1.15/bin/svn fixtures/wc/make-wc-fixtures.sh 31
{ pkgs ? import <nixpkgs> { } }:
pkgs.subversion.overrideAttrs (old: rec {
  version = "1.15.0";
  src = pkgs.fetchurl {
    url = "https://archive.apache.org/dist/subversion/subversion-${version}.tar.bz2";
    sha512 = "6bb79686baab7176e9b6c519b23141f66694298c0759d1e12332fef329eaa632e4f2e65c75ae0cd05e48e16c23d7eda7c59b322bdf03707e1a46025673282ec2";
  };
  # nixpkgs patches target 1.14.x; 1.15.0 builds without them.
  patches = [ ];
  doCheck = false;
})

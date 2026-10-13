#!/bin/sh
# Generate a throwaway test CA and a server certificate under ${1:-/certs}.
# Idempotent: does nothing when the certificate already exists. Test-only material; the
# private keys are world-readable so unprivileged containers and the host can read them.
set -eu
dir="${1:-/certs}"
mkdir -p "$dir"
if [ -s "$dir/server.crt" ] && [ -s "$dir/ca.crt" ]; then
  echo "certs already present in $dir"
  exit 0
fi
cd "$dir"
openssl genrsa -out ca.key 2048 2>/dev/null
openssl req -x509 -new -key ca.key -sha256 -days 3650 -subj "/CN=OpenSVN Compat Test CA" -out ca.crt
openssl genrsa -out server.key 2048 2>/dev/null
openssl req -new -key server.key -subj "/CN=svn.test.local" -out server.csr
cat > server.ext <<EXT
subjectAltName=DNS:svn.test.local,DNS:svn14.test.local,DNS:svn15.test.local,DNS:localhost,IP:127.0.0.1
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
EXT
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -sha256 -days 825 \
  -extfile server.ext -out server.crt
rm -f server.csr server.ext ca.srl
chmod 644 ./*.crt ./*.key
echo "generated test CA and server certificate in $dir"

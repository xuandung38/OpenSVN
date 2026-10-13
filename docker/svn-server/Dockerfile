# syntax=docker/dockerfile:1.7
# Apache httpd 2.4 + mod_dav_svn compiled from the official Subversion source tarball.
# One Dockerfile for every pinned version: version and tarball checksum are build args
# (see docker/compose.yml). Layers are ordered so the slow compile stays cached until the
# version pin changes; config files are copied last.

FROM debian:bookworm-slim AS compile
ARG SVN_VERSION
ARG SVN_SHA512
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl bzip2 pkg-config \
      apache2-dev libapr1-dev libaprutil1-dev libserf-dev libsqlite3-dev \
      libexpat1-dev zlib1g-dev libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# Tarball is verified against the SHA-512 published in the Apache release record.
RUN curl -fsSL -o svn.tar.bz2 "https://archive.apache.org/dist/subversion/subversion-${SVN_VERSION}.tar.bz2" \
    && echo "${SVN_SHA512}  svn.tar.bz2" | sha512sum -c - \
    && tar -xjf svn.tar.bz2 --strip-components=1 && rm svn.tar.bz2
RUN ./configure --prefix=/usr/local --with-apxs=/usr/bin/apxs --with-serf \
      --with-lz4=internal --with-utf8proc=internal --disable-mod-activation \
      --without-berkeley-db --without-swig --disable-nls \
    && make -j"$(nproc)"
# Separate layer: install problems do not force a recompile. With DESTDIR, apxs places
# mod_dav_svn.so / mod_authz_svn.so under /usr/local/libexec.
RUN make install DESTDIR=/out && ls /out/usr/local/libexec/mod_dav_svn.so

FROM debian:bookworm-slim
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends \
      apache2 apache2-utils openssl ca-certificates curl \
      libapr1 libaprutil1 libserf-1-1 libsqlite3-0 libexpat1 zlib1g libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=compile /out/usr/local/ /usr/local/
RUN ldconfig
# Test-only Basic credentials. Never reuse anywhere real.
RUN htpasswd -cbB /etc/svn-htpasswd tester tester-pass \
    && mkdir -p /var/svn/repos /var/log/svn-httpd /certs
COPY httpd.conf /etc/svn-httpd.conf
COPY entrypoint.sh /usr/local/bin/svn-server-entrypoint
EXPOSE 8443
HEALTHCHECK --interval=3s --timeout=3s --start-period=5s --retries=15 \
  CMD curl -fs --cacert /certs/ca.crt -u tester:tester-pass https://localhost:8443/svn/ -o /dev/null || exit 1
ENTRYPOINT ["svn-server-entrypoint"]

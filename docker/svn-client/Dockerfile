# syntax=docker/dockerfile:1.7
# svn command-line client compiled from the official source tarball (version + SHA-512 pinned
# via build args). Trusts the compat test CA; also ships openssl for gen-certs.sh and sqlite3.
FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587 AS compile
ARG SVN_VERSION
ARG SVN_SHA512
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl bzip2 pkg-config \
      libapr1-dev libaprutil1-dev libserf-dev libsqlite3-dev libexpat1-dev zlib1g-dev libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN curl -fsSL -o svn.tar.bz2 "https://archive.apache.org/dist/subversion/subversion-${SVN_VERSION}.tar.bz2" \
    && echo "${SVN_SHA512}  svn.tar.bz2" | sha512sum -c - \
    && tar -xjf svn.tar.bz2 --strip-components=1 && rm svn.tar.bz2
RUN ./configure --prefix=/usr/local --with-serf --without-apxs \
      --with-lz4=internal --with-utf8proc=internal \
      --without-berkeley-db --without-swig --disable-nls \
    && make -j"$(nproc)" && make install DESTDIR=/out

FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends \
      openssl ca-certificates sqlite3 \
      libapr1 libaprutil1 libserf-1-1 libsqlite3-0 libexpat1 zlib1g libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=compile /out/usr/local/ /usr/local/
RUN ldconfig
COPY servers /etc/subversion/servers
ENV HOME=/tmp/home
WORKDIR /work
CMD ["sleep", "infinity"]

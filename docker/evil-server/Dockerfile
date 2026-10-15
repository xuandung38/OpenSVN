# syntax=docker/dockerfile:1.7
# Evil server: Apache httpd + mod_dav_svn over HTTPS serving the hostile repository of
# fixtures/evil/make-hostile-repo.sh. The container runs docker/evil-server/run-local.sh, which
# also works without Docker (see fixtures/evil/README.md).
#
#   docker build -f docker/evil-server/Dockerfile -t opensvn-compat/evil-server .
#   docker run --rm -p 127.0.0.1:18543:18543 opensvn-compat/evil-server
#   -> https://localhost:18543/svn/hostile  (tester / tester-pass; CA at /evil/certs/ca.crt)
#
# Build context: the repository root (the builder and the certificate script are copied in).
FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 LC_ALL=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends \
      apache2 libapache2-mod-svn subversion openssl ca-certificates curl python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /opensvn
COPY docker/svn-server/gen-certs.sh docker/svn-server/gen-certs.sh
COPY docker/evil-server/run-local.sh docker/evil-server/run-local.sh
COPY fixtures/repos/lib.sh fixtures/repos/lib.sh
COPY fixtures/evil/make-hostile-repo.sh fixtures/evil/make-hostile-repo.sh
# Listen on all interfaces inside the container; publish on 127.0.0.1 only (see above).
RUN sed -i 's|^Listen 127.0.0.1:|Listen |' docker/evil-server/run-local.sh
ENV EVIL_DIR=/evil EVIL_PORT=18543
EXPOSE 18543
HEALTHCHECK --interval=3s --timeout=3s --start-period=10s --retries=20 \
  CMD curl -fs --cacert /evil/certs/ca.crt -u tester:tester-pass https://localhost:18543/svn/hostile/ -o /dev/null || exit 1
ENTRYPOINT ["bash", "docker/evil-server/run-local.sh", "foreground"]

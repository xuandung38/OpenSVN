# Throwaway Apache + mod_dav_svn used only to record golden protocol traffic.
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      apache2 libapache2-mod-svn subversion apache2-utils ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY dav_svn.conf /etc/apache2/conf-available/capture-svn.conf
COPY entrypoint.sh /entrypoint.sh
RUN a2enmod dav dav_svn authz_svn auth_basic authn_file && a2enconf capture-svn \
    && chmod +x /entrypoint.sh
EXPOSE 80
ENTRYPOINT ["/entrypoint.sh"]

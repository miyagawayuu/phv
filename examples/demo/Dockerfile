# The public demo at https://demo.phv-lang.org: the released phv binary
# serving this directory. Build from examples/demo/ (fly deploy does).

# The release binary is built on Ubuntu 24.04, so it needs its glibc. The
# image is pinned by digest (Dependabot keeps it current).
FROM ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852 AS fetch
ARG PHV_VERSION=1.2.1
# The archive's SHA-256, written here when the version is set (from the
# SHA256SUMS checked before the release was published), so a release asset
# replaced later is refused.
ARG PHV_SHA256=3a4fdda51f0df9ef8df725b4335ccd836d78c807838a6002a1a118c3551063fc
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /tmp/phv
RUN name="phv-${PHV_VERSION}-x86_64-unknown-linux-gnu" \
 && base="https://github.com/miyagawayuu/phv/releases/download/v${PHV_VERSION}" \
 && curl -fsSLO "$base/$name.tar.gz" \
 && echo "$PHV_SHA256  $name.tar.gz" | sha256sum -c - \
 && tar xzf "$name.tar.gz" \
 && install -m 755 "$name/phv" /usr/local/bin/phv

FROM ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
COPY --from=fetch /usr/local/bin/phv /usr/local/bin/phv
RUN useradd --system --create-home --home-dir /home/phv phv
WORKDIR /app
# The source stays owned by root: the server cannot change its own code.
COPY . .
RUN mkdir -p data .phv-cache \
 && chown phv:phv data .phv-cache \
 && chmod 750 data .phv-cache
COPY --chmod=755 entrypoint.sh /usr/local/bin/entrypoint.sh
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["phv", "serve", ".", "--host", "0.0.0.0", "--port", "8080", "--prod"]

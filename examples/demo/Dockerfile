# The public demo at https://demo.phv-lang.org: the released phv binary
# serving this directory. Build from examples/demo/ (fly deploy does).

# The release binary is built on Ubuntu 24.04, so it needs its glibc.
FROM ubuntu:24.04 AS fetch
ARG PHV_VERSION=1.1.0
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /tmp/phv
RUN name="phv-${PHV_VERSION}-x86_64-unknown-linux-gnu" \
 && base="https://github.com/miyagawayuu/phv/releases/download/v${PHV_VERSION}" \
 && curl -fsSLO "$base/$name.tar.gz" \
 && curl -fsSLO "$base/SHA256SUMS" \
 && grep "  $name.tar.gz\$" SHA256SUMS | sha256sum -c - \
 && tar xzf "$name.tar.gz" \
 && install -m 755 "$name/phv" /usr/local/bin/phv

FROM ubuntu:24.04
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

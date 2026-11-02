# The public demo at https://demo.phx-lang.org: the released phx binary
# serving this directory. Build from examples/demo/ (fly deploy does).

# The release binary is built on Ubuntu 24.04, so it needs its glibc.
FROM ubuntu:24.04 AS fetch
ARG PHX_VERSION=0.2.4
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /tmp/phx
RUN name="phx-${PHX_VERSION}-x86_64-unknown-linux-gnu" \
 && base="https://github.com/miyagawayuu/phx/releases/download/v${PHX_VERSION}" \
 && curl -fsSLO "$base/$name.tar.gz" \
 && curl -fsSLO "$base/SHA256SUMS" \
 && grep "  $name.tar.gz\$" SHA256SUMS | sha256sum -c - \
 && tar xzf "$name.tar.gz" \
 && install -m 755 "$name/phx" /usr/local/bin/phx

FROM ubuntu:24.04
COPY --from=fetch /usr/local/bin/phx /usr/local/bin/phx
RUN useradd --system --create-home --home-dir /home/phx phx
WORKDIR /app
# The source stays owned by root: the server cannot change its own code.
COPY . .
RUN mkdir -p data .phx-cache \
 && chown phx:phx data .phx-cache \
 && chmod 750 data .phx-cache
COPY --chmod=755 entrypoint.sh /usr/local/bin/entrypoint.sh
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["phx", "serve", ".", "--host", "0.0.0.0", "--port", "8080", "--prod"]

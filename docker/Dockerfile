# The PHV image: the static (musl) `phv` executable and nothing else. The
# release workflows put the executable of each architecture in `<arch>/phv`
# next to this file (`amd64`, `arm64`) and an empty `tmp` directory.
#
#   docker run --rm -p 8080:8080 -v "$PWD:/app" --user "$(id -u):$(id -g)" \
#     ghcr.io/miyagawayuu/phv serve --host 0.0.0.0 --prod
#
# There is no shell or other program in it; TLS roots are built into phv.
FROM scratch
ARG TARGETARCH
ARG VERSION=dev
LABEL org.opencontainers.image.title="PHV" \
      org.opencontainers.image.description="The PHV programming language: phv run, serve, test, build..." \
      org.opencontainers.image.source="https://github.com/miyagawayuu/phv" \
      org.opencontainers.image.url="https://phv-lang.org" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}"
COPY --chmod=0755 ${TARGETARCH}/phv /usr/local/bin/phv
COPY --chmod=1777 tmp /tmp
ENV TMPDIR=/tmp
WORKDIR /app
# Not root: a site is served by an unprivileged user (pass --user to write to a mounted directory).
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/phv"]
CMD ["--help"]

#!/bin/sh
# Fly.io mounts the volume at /app/data owned by root. Hand it to the phx
# user, then drop root before starting the server.
set -eu
chown phx:phx /app/data
chmod 750 /app/data
exec setpriv --reuid=phx --regid=phx --init-groups "$@"

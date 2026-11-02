#!/bin/sh
# Fly.io mounts the volume at /app/data owned by root. Hand it to the phv
# user, then drop root before starting the server.
set -eu
chown phv:phv /app/data
chmod 750 /app/data
exec setpriv --reuid=phv --regid=phv --init-groups "$@"

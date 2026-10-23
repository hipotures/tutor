#!/bin/sh
# Entrypoint of the HomeStack image. /home/app is the instance's persistent
# volume, attached by HomeStack before the first start. Prepare its
# directories without touching existing data, then start Vaultwarden.
set -eu

APP_HOME=/home/app

if [ -L "${APP_HOME}" ] || [ ! -d "${APP_HOME}" ]; then
    echo "${APP_HOME} must be a directory (the persistent volume), not a symlink or missing" >&2
    exit 1
fi

# Require this exact mount point. A bind mount may share the root's device,
# while an ordinary directory on another nested filesystem is not the home mount.
if [ "${I_REALLY_WANT_VOLATILE_STORAGE:-}" != true ] && ! awk '
    $5 == "/home/app" { mounted = 1 }
    END { exit !mounted }
' /proc/self/mountinfo; then
    echo "${APP_HOME} is not a mounted volume; refusing to keep the vault on the disposable root." >&2
    echo "Mount a persistent volume at ${APP_HOME}, or set I_REALLY_WANT_VOLATILE_STORAGE=true for a throwaway test." >&2
    exit 1
fi

mkdir -p "${APP_HOME}/data"

exec /start.sh "$@"

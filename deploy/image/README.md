# Tutor image for HomeStack

Tutor (this Vaultwarden fork) is deployed by HomeStack as one unprivileged
Proxmox LXC created from the OCI image `ghcr.io/hipotures/tutor`. The service
definition is [`.homestack/tutor.yaml`](../../.homestack/tutor.yaml); the
manifest schema is
[`.agents/skills/homestack-service/manifest-schema.md`](../../.agents/skills/homestack-service/manifest-schema.md).

## Releases

`python3 scripts/tag-image.py` tags the clean, already pushed HEAD as the next
patch version `tutor-X.Y.Z` and pushes the tag (`--dry-run` only prints it).
The tag runs `.github/workflows/tutor-image.yml`, which builds
[`Containerfile`](Containerfile) for `linux/amd64` and publishes
`ghcr.io/hipotures/tutor:X.Y.Z`; it refuses to overwrite a version that is
already published. The `tutor-` prefix keeps fork releases apart from upstream
Vaultwarden tags. Only `X.Y.Z` image tags are releases; a manual workflow run
publishes `dev-<commit>` images for testing, and ordinary pushes publish
nothing. The package is public, so a Proxmox node pulls it without
credentials.

`tutor-0.1.0` has no image (its build was cancelled) and `0.1.1` predates the
`/home/app/data` layout; use later releases.

## Runtime contract

| | |
| :--- | :--- |
| Entrypoint | `/entrypoint.sh`, which prepares `/home/app` and runs `/start.sh` → `/vaultwarden` |
| Port | `80/tcp` (`web`), plain HTTP |
| Health | `GET /alive` → `200`; it also checks the database connection |
| Persistent volume | `/home/app`, mounted by HomeStack before the first start |
| State | `/home/app/data` (`DATA_FOLDER`): `db.sqlite3` with its `-wal`/`-shm` files, `rsa_key.pem`, `config.json`, `attachments/`, `sends/`, `icon_cache/`, `tmp/` |
| Logs | stdout/stderr |
| User | container root (the LXC is unprivileged, so it maps to an unprivileged host UID); the volume is owned by container root |
| Instances | one: local SQLite (`multi_node: false`, `state_model: local`) |

The entrypoint refuses to start if `/home/app` is missing, a symlink, or not a
separate volume (same device as `/`), so the vault never ends up on the
disposable root. `I_REALLY_WANT_VOLATILE_STORAGE=true` overrides this for
throwaway tests. It only creates `/home/app/data` when missing and never
changes existing data.

Not one instance per node: several instances would be separate vaults with
separate databases, not replicas.

## Settings provided at deployment

These are Vaultwarden environment variables set on the container by the
deployment, never in the image or manifest:

- `DOMAIN`: the public `https://` URL. The web vault needs HTTPS (a secure
  context), so the container is reached through a TLS-terminating proxy.
- `ADMIN_TOKEN` (optional, secret): enables `/admin`.
- `EXPERIMENTAL_CLIENT_FEATURE_FLAGS=vw-api-key-item-type` (optional): enables
  the fork's API key item type, only with clients that support it.

## Backups and migration

Back up `/home/app/data` with SQLite's online backup (`sqlite3 db.sqlite3
".backup /path/backup.sqlite3"`) plus the other files listed above; never copy
a live `db.sqlite3` with `cp`. Moving an existing Vaultwarden into Tutor means
restoring such a backup into the new instance's `/home/app/data` while Tutor is
stopped.

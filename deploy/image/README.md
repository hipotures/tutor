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
already published. If the tagged commit already has a `dev-<commit>` image
(from a manual run), the workflow copies that tested image to `X.Y.Z`
instead of rebuilding; it then reports the dev build's version
(`tutor-dev-<commit>`). The `tutor-` prefix keeps fork releases apart from upstream
Vaultwarden tags. Only `X.Y.Z` image tags are releases; a manual workflow run
publishes `dev-<commit>` images for testing, and ordinary pushes publish
nothing. The package is public, so a Proxmox node pulls it without
credentials.

`tutor-0.1.0` has no image (its build was cancelled) and `0.1.1` predates the
`/home/app/data` layout; use later releases.

## Runtime contract

The runtime uses Alpine 3.24: the server is built for `x86_64-unknown-linux-musl`
and needs neither a compiler nor Node.js at runtime. Alpine supplies the shell
used by the existing startup scripts, CA certificates for outbound TLS,
OpenSSL, timezone data and curl for the existing image healthcheck. Build tools
and web vault compilation stay in separate stages. Production image builds run
in CI; never run the production web vault build on the development VM.

| | |
| :--- | :--- |
| Entrypoint | `/entrypoint.sh`, which prepares `/home/app` and runs `/start.sh` → `/vaultwarden` |
| Port | `80/tcp` (`web`), plain HTTP |
| Health | `GET /health/live` reports heartbeat freshness; `GET /health/ready` additionally checks the database with `SELECT 1`. Both return `application/health+json`, `200` with `"status":"pass"` or `503` with `"status":"fail"`. Both paths are served at root even with a `DOMAIN` base path; the prefixed paths remain available. The upstream `/alive` stays for the Docker healthcheck |
| Telemetry | `GET /.well-known/homestack/status` on port 80, always at root: JSON schema 2 with process uptime, mount capacities and the service's own cgroup v2 CPU, memory, PSI and I/O counters. CPU and I/O rates are calculated by HomeStack. The status verdict matches `/health/live`; `/proc/loadavg` is never read. For the management network only: a public proxy must not route this path |
| Persistent volume | `/home/app`, mounted by HomeStack before the first start |
| State | `/home/app/data` (`DATA_FOLDER`): `db.sqlite3` with its `-wal`/`-shm` files, `rsa_key.pem`, `config.json`, `attachments/`, `sends/`, `icon_cache/`, `tmp/` |
| Web vault | `/web-vault`, built from source with the fork's patches (`deploy/web-vault-patches/`) |
| Logs | stdout/stderr |
| User | container root (the LXC is unprivileged, so it maps to an unprivileged host UID); the volume is owned by container root |
| Instances | one: local SQLite (`instances: 1`, `placement: single`, `state_model: local`), hostname `tutor` by default |

The entrypoint refuses to start if `/home/app` is missing, a symlink, or not
an exact mount point in `/proc/self/mountinfo`, so the vault never ends up on the
disposable root. `I_REALLY_WANT_VOLATILE_STORAGE=true` bypasses only the mount check for
throwaway tests; other values, including `false`, do not. Bind mounts on the
same filesystem as root are accepted. It only creates `/home/app/data` when missing and never
changes existing data.

Not one instance per node: several instances would be separate vaults with
separate databases, not replicas.

## Heartbeat and telemetry lifecycle

At listener liftoff, one async heartbeat task sends a request to the service's
own `/health/heartbeat` route. That route obtains an ordinary database
connection and reads `SELECT 1`; it writes no application records. The probe
is independent of heartbeat freshness so recovery does not require a restart.
Only a successful HTTP `204` increments the count and updates the monotonic
time of the last success. The client has a two-second timeout, disables
proxies and redirects, and stops at application shutdown.

`HOMESTACK_HEARTBEAT_SECONDS` sets the interval (10–3600, default 60).
Both health responses include `heartbeat:age` in integer seconds and
`heartbeat:count`; after more than three intervals without success they
return `503`. Before the first success, age is measured from process start.
Readiness also requires a successful database read. Telemetry reports the
same liveness verdict even when disk measurements are unavailable.

Telemetry has no timer. One worker waits until a status request arrives.
Requests wait at most two seconds and share an in-flight collection; requests
within two seconds of completion reuse it. A stuck filesystem call cannot
spawn another worker. While it is stuck, responses retain the previous
measurement and its actual age, or report `pending` before any result exists.
`sample_uptime_usec` timestamps collection start relative to process start.
Without status requests there are no telemetry reads.

## Resource validation

The manifest currently declares provisional defaults: 1 core, 256 MiB RAM,
no swap, 512 MiB rootfs and a 1 GiB home volume. These are not measurements of
the production image. Resource sizing, representative load at those limits,
and persistence across replacement with a fresh container remain unverified.
Do not infer image resource requirements from local Rust tests or development
builds. Record the image digest, workload, cgroup peaks/OOM events and actual
root/home usage here when a container runtime is available for those checks.

## Local verification

The HomeStack changes were checked with `CARGO_BUILD_JOBS=2 cargo check
--features sqlite` and 15 targeted `cargo test --features sqlite homestack`
tests. They cover request coalescing, an idle worker, two-second timeouts,
stale and pending measurements, cgroup counters/limits, mount failures and
heartbeat expiry/recovery. An actual HTTP heartbeat test blocks its serving
path and verifies that both health endpoints fail, then recover when it is
released.

A local debug binary was also run with a fresh throwaway SQLite directory,
`DOMAIN` containing `/vault` and a ten-second heartbeat. HTTP checks verified
heartbeat growth, health at both root and the base path, schema 2, filesystem
capacities against native `statvfs`, cache reuse and unchanged telemetry
thread I/O counters while no requests arrived. Restarting the process kept
its key and database. Shell fixture checks covered exact mounts, same-device
bind mounts, symlinks, reused data and the explicit volatile-storage opt-in.
These checks do not establish production image resource sizes or LXC behavior.

## Settings provided at deployment

These are Vaultwarden environment variables set on the container by the
deployment, never in the image or manifest:

- `DOMAIN`: the public `https://` URL. The web vault needs HTTPS (a secure
  context), so the container is reached through a TLS-terminating proxy.
- `ADMIN_TOKEN` (optional, secret): enables `/admin`.
- `HOMESTACK_HEARTBEAT_SECONDS`: supplied by HomeStack from the manifest
  healthcheck; defaults to 60 when absent. Invalid values fall back to 60.
- `EXPERIMENTAL_CLIENT_FEATURE_FLAGS=vw-api-key-item-type` (optional): enables
  the fork's API key item type, only with clients that support it.

## Backups and migration

Back up `/home/app/data` with SQLite's online backup (`sqlite3 db.sqlite3
".backup /path/backup.sqlite3"`) plus the other files listed above; never copy
a live `db.sqlite3` with `cp`. Moving an existing Vaultwarden into Tutor means
restoring such a backup into the new instance's `/home/app/data` while Tutor is
stopped.

# Agent instructions

This repository is a fork of Vaultwarden deployed by HomeStack as the `tutor`
service. The HomeStack contract is in `.agents/skills/homestack-service/`; the
service's runtime contract is in `deploy/image/README.md`.

## Local development checks

For Rust compilation checks, default to
`CARGO_BUILD_JOBS=2 cargo check --features sqlite`. For behavior changes, also
run the relevant targeted test, for example
`CARGO_BUILD_JOBS=2 cargo test --features sqlite <test_name>`.
Do not run `cargo build --release` merely to validate a small change; build a
binary only when needed to run or package it. Reuse the same Cargo profile for
routine checks so incremental artifacts remain useful.

For local web vault work on this 8 GiB VM, use the development launcher:

```sh
python3 scripts/web-vault/dev.py \
  --source /home/user/webvault-build \
  --node /home/user/.local/node24/bin/node
```

It watches by default; add `--mode once` for a single build. It caps the entire
build cgroup at 4 GiB and disables swap for that cgroup.

Never run a production web vault build on this development VM: it has already
caused a global OOM. This includes `npm run dist:oss:selfhost`,
`npm run build:oss:selfhost:prod`, webpack with `NODE_ENV=production`, and local
Docker/Podman builds of `deploy/image/Containerfile` (which build the production
web vault internally). Production image builds run in CI. Do not use a
production build to check a local fix. Do not substitute an unlimited
webpack/npm build, increase VM RAM, or add swap as a workaround.

The external source checkout contains existing Tutor patches; preserve them.
Use `scripts/web-vault/README.md` for configuration, measurements, limitations,
and the focused SDK fallback check. Keep these development settings separate
from the production image build. A completed build checks compilation; verify
the affected behavior separately when required.

## Image releases

After committing and pushing a completed change, decide whether it changes the
image: server code or its dependencies, migrations, the `deploy/image/`
Containerfile, entrypoint or other files baked into the image. If it does,
release it with `python3 scripts/tag-image.py`, which picks the next
`tutor-X.Y.Z` and pushes the tag; CI publishes `ghcr.io/hipotures/tutor:X.Y.Z`.
HomeStack reads the manifest from the repository: changes only to
`.homestack/tutor.yaml` (resources, persistence size, instances, placement or
exposure) need no image release. Do not release changes confined to
documentation, tests, CI checks, upstream workflows or tooling.

Never pick versions by hand, and never move, delete or force-push release tags.
To test an image without releasing it, run the `Tutor image` workflow manually
(it publishes `dev-<commit>`).

When a `dev-<commit>` image has been tested successfully (by your own checks,
or by the user confirming it works), release that same commit with
`tag-image.py` without new commits in between: CI then copies the tested
`dev-<commit>` image to `X.Y.Z` instead of rebuilding it. A promoted image keeps
the dev build's embedded version (`tutor-dev-<commit>`) and labels. A release
of a commit without a dev image is built from scratch.

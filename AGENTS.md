# Agent instructions

This repository is a fork of Vaultwarden deployed by HomeStack as the `tutor`
service. The HomeStack contract is in `.agents/skills/homestack-service/`; the
service's runtime contract is in `deploy/image/README.md`.

## Image releases

After committing and pushing a completed change, decide whether it changes the
running service: server code or its dependencies, migrations, the
`deploy/image/` Containerfile, entrypoint or runtime files, or the `image`,
`runtime` or `persistence` sections of `.homestack/tutor.yaml`. If it does,
release it with `python3 scripts/tag-image.py`, which picks the next
`tutor-X.Y.Z` and pushes the tag; CI publishes `ghcr.io/hipotures/tutor:X.Y.Z`.
Do not release changes confined to documentation, tests, CI checks, upstream
workflows or tooling.

Never pick versions by hand, and never move, delete or force-push release tags.
To test an image without releasing it, run the `Tutor image` workflow manually
(it publishes `dev-<commit>`).

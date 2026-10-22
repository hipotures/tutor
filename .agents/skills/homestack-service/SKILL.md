---
name: homestack-service
description: Prepare an application repository for deployment by HomeStack as a Proxmox LXC built from an OCI image, with persistent data in /home/app. Use when asked to make a service deployable through HomeStack, to write or review .homestack/*.yaml manifests, or to adapt a Containerfile/entrypoint for a persistent /home/app volume.
---

# HomeStack service authoring

HomeStack deploys a service as one unprivileged Proxmox LXC per selected node,
created from the service's published OCI image, with a dedicated persistent
volume mounted at `/home/app`. You prepare the application side only: the
image, its runtime data layout and a portable manifest. The exact manifest
contract is in [manifest-schema.md](manifest-schema.md); follow it literally.

## Boundary

- You own: the Containerfile, the entrypoint, where the application writes
  state, the release workflow that publishes the image, and `.homestack/*.yaml`.
- HomeStack owns: nodes, CTIDs, IPs, storage, volume sizes, CPU/RAM, domains,
  exposure and every secret. Never put any of these in the repository, the
  image or the manifest, and never run Proxmox operations yourself.

## Steps

1. Read the repository's own instructions and the real runtime: how the image
   is built and published, the start command, listening ports, a real health
   endpoint, and every path the application writes. Do not invent any of them.
2. Decide what is one service: one independently deployable LXC. Several
   processes or ports in one container are one service with several ports.
   Do not add a manifest for something that is not deployable on its own.
3. Put all persistent state under `/home/app/<directory>`:
   - prefer the application's own setting (an environment variable or config
     key pointing at `/home/app/data`) over symlinks;
   - if a path cannot be configured, have the entrypoint link it into the
     mount, and check the link never hides or replaces existing data;
   - SQLite databases and their `-wal`/`-shm` files stay together inside the
     mount; never copy a live database with `cp`;
   - logs go to stdout/stderr unless there is a real reason to keep them.
4. Make the image safe for an empty, then reused, volume:
   - keep `/home/app` an empty directory in the image; nothing baked there
     survives the mount;
   - in the entrypoint, create the needed directories with `mkdir -p` after
     the mount is present, never reinitialise or overwrite existing data;
   - refuse to start when `/home/app` is a symlink or not a mounted volume,
     unless an explicit opt-in for throwaway tests is set;
   - do not declare `VOLUME` and do not mount anything in the image;
   - the LXC is unprivileged: run as container root or a fixed service user
     that owns its directories; never fix permissions with world-writable modes.
5. Keep the existing build and release process when it works. The published
   image must be single `linux/amd64`, pullable anonymously by the Proxmox
   node, and released under plain `MAJOR.MINOR.PATCH` tags. Do not create
   release tags unless the repository's own instructions say so.
6. Write `.homestack/<id>.yaml` per the schema. `multi_node: true` only when
   independent simultaneous instances are genuinely safe (`stateless`, or
   `external` shared state designed for it). Local SQLite means
   `multi_node: false` and `state_model: local`. A health check must be real:
   use HTTP only if the service serves that path; otherwise `tcp` or none.
7. Verify without touching any cluster: the YAML parses and satisfies the
   schema; the service starts with an empty `/home/app`, writes its state
   there, and keeps it across a restart.
8. Document the service's runtime contract next to its image, including the
   settings HomeStack must provide at deployment time (never their values).

## Refuse

Cluster-specific values or secrets in the manifest or image, shell commands in
manifests, Docker-in-LXC, and claiming multi-node safety or health that the
application does not actually have.

# Web vault patches

The image builds the web vault from `vaultwarden/vw_web_builds` (Bitwarden's
clients with Vaultwarden's changes) at the commit pinned in
[`../image/Containerfile`](../image/Containerfile), then applies these patches
with `git apply` in name order (`*.patch`).

- `0001-vault-filter-item-counts.patch`: shows the number of items next to
  each type filter (favorites, item types, archive, trash) in the individual
  vault, in muted text. Counts follow the selected vault, like the item list.

A patch is a `git diff` from the root of that checkout. When moving to a new
web vault version, update the commit and refresh any patch that no longer
applies.

# AGENTS.md — Repo hygiene

Scope: this file covers *repo hygiene* — branching, remotes, CI, cleanup. It is not project documentation.

## Branch model
- Single-trunk: `main` only. No `dev` or `beta` branch. Land small changes directly, or use short-lived `feature/x`/`fix/x` branches for anything non-trivial and merge back to `main`.
- This replaced an earlier three-branch (`dev`/`beta`/`main`) model after `main` silently rotted across the ecosystem. Don't recreate those branches.

## Channel
- **Pacman-only, permanently.** There is no `bakery.toml` on purpose: breadlock installs a root-owned `/etc/pam.d/breadlock` PAM service (and `breadgreet` is a greetd greeter). Bakery has no privileged-install path. Do not add `bakery.toml`.
- Releases are `v*` tags. `.forgejo/workflows/package.yml` builds the `[breadway]` pacman package. There are no bakery tracks (`dev`/`beta`/`stable` indexes) for this repo.

## Remotes
- `origin` — Forgejo (`git.breadway.dev` via Hestia, SSH) — authoritative. Push here.
- `github` — GitHub mirror (push-mirror; do not push to it by hand).

## CI
- `.forgejo/workflows/package.yml` triggers only on `push: tags: ['v*']` — regular pushes to `main` run nothing. Tag a release to trigger packaging.
- No build/lint/test CI runs on ordinary commits or PRs — test locally before merging.

## Cleanup
- Delete feature/fix branches (local + remote) once merged. Check with `git branch --merged main`.

## Don't
- Don't add `bakery.toml`.
- Don't embed credentials in remote URLs — SSH or a credential helper only.

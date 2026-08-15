# Contributing

`breadlock` / `breadgreet` — session locker and greetd greeter for Hyprland.

Single-trunk, same as the rest of the ecosystem: one long-lived branch
(`main`), short-lived `feature/<name>` / `fix/<name>` branches, merge back.
No `dev` or `beta` branch.

This repo is a deliberate **pacman-only** exception. There is no
`bakery.toml` — breadlock needs a root-owned `/etc/pam.d/breadlock` PAM
service, which bakery cannot install. Don't add one. Releases are `v*`
tags that fire `.forgejo/workflows/package.yml` into the `[breadway]`
pacman repo. There are no bakery tracks.

See `AGENTS.md` for remotes and CI details.

## Local development

```sh
cargo build --release --bin breadlock --bin breadgreet
cargo test --workspace
```

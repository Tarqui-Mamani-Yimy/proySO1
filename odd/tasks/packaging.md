# Packaging Shellux and Nulix (sugPerms.md item 8)

Objective: anyone installs Shellux or Nulix with one command (Homebrew, AUR or an install script) from prebuilt GitHub Release binaries; nothing is published without the user's explicit authorization.
Branch: `feat/packaging`. Delivery: merge to main on the user's request; publishing (tag/release, Homebrew tap, AUR) is a separate, explicitly authorized step. Test runner: `sh scripts/package.sh` + `sh scripts/test-install.sh` locally, `make ci`.

## Specs

- **S0** — sugPerms.md item 8 (verbatim): "**Empaquetado** (binarios, AUR, Homebrew). Depende de decidir la licencia (`planing.md` §6)." License: MIT (LICENSE holds both copyrights; crates/epsh/LICENSE kept).
- **S1** — User framing (verbatim): "o sea que en otras palabras es prepararlo para que cualquier persona con un solo comando pueda hacer la instalacion del proyecto, veradad" — answer was yes.
- **S2** — Scope chosen (verbatim option): "Dos paquetes: shellux y nulix (Recommended)" — "Paquete `shellux` (shellux + epsh) y paquete `nulix` (nulix, nulix-server, nulixctl; depende de shellux). Linux x86_64 y macOS arm64."
- **S3** — Release artifacts per version `X.Y.Z` and target (`x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`): `shellux-X.Y.Z-<target>.tar.gz` (bin/shellux, bin/epsh, LICENSE, LICENSE-epsh, README.md) and `nulix-X.Y.Z-<target>.tar.gz` (bin/nulix, bin/nulix-server, bin/nulixctl, LICENSE, README.md, nulix.conf.example), each with a `.sha256` file. Built by `scripts/package.sh`, used both locally and in CI.
- **S4** — CI: `.github/workflows/ci.yml` runs `make ci` on pushes and pull requests; `.github/workflows/release.yml` on tags `v*` builds both targets, runs the tests, packages, and attaches the artifacts to a GitHub Release.
- **S5** — One-command installers: `install.sh` (downloads the matching tarball from the latest or a given release, verifies its sha256, installs to `$PREFIX/bin`, default `~/.local/bin`; `PACKAGE=shellux|nulix`), Homebrew formulas `shellux` and `nulix` (nulix depends on shellux) and AUR `shellux-bin` / `nulix-bin` PKGBUILDs, rendered with the release version and checksums by `scripts/render-packaging.sh`.
- **S6** — Docs: README install section and AGENTS.md packaging notes.

## Tasks

| ID | Specs | Route | Status | Commit |
|---|---|---|---|---|
| T1 — package.sh + install.sh with local tests | S3, S5 | inline | done | 82a535c |
| T2 — CI and release workflows | S4 | inline | done (not run on GitHub yet) | 82a535c |
| T3 — Homebrew/AUR templates + render script, docs | S5, S6 | inline | done | 82a535c |

## Log

- **L1** (2026-11-22, user): "ok, hagamos eso" (start item 8 with the scope decision).
- **L2** (2026-11-22, user answer): "Dos paquetes: shellux y nulix (Recommended)".
- **L3**: epsh attribution already satisfied (LICENSE lists Joshua Li and Yimy Tarqui Mamani; crates/epsh/LICENSE is MIT). Nulix links Wayland/X11 at run time (wayland-sys, x11-dl); CI needs pkg-config and X11/xkbcommon/Wayland headers.
- **L4**: `sh scripts/test-install.sh` passes all 23 checks locally (tarball contents, checksums, install of both packages from file://, installed binaries run, tampered checksum / unsupported platform / unknown package refused, rendered formulas and AUR files with no placeholders, PKGBUILD package() installs the expected files). Workflows parse as YAML; the tag/version check was run locally for a matching and a mismatching tag. Not verified locally: the workflows on GitHub runners, the macOS build and tests, and the Homebrew formulas (no ruby/brew here). The GitHub repo is public and Actions are enabled. Risk: item 5 (CI, release, installers) → independent verifier.
- **L5**: Independent verifier: no blockers; S0–S6 met locally (S4 by reading). Applied its notes: workflows run `rustup toolchain install` explicitly (rustup 1.28+ `rustup show` no longer installs); install.sh accepts `VERSION=vX.Y.Z`, reports "has no published release yet" when /releases/latest redirects to /releases, exits on INT/TERM, and stages every binary before renaming any; Homebrew formulas declare `depends_on arch:` per OS (no confusing failure on Intel Mac / arm64 Linux) and drop the redundant `version`; nulix-bin optdepends add libxi, libxkbcommon-x11, libglvnd. scripts/test-install.sh now has 29 checks (missing .sha256, leading v, amd64 alias, PREFIX with a space, shasum-only PATH, relative OUT), all passing. Still unverified: GitHub runners, macOS, brew audit/makepkg.

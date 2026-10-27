# Publishing Ampwire to the AUR

Status: **prepared, not published.** The PKGBUILD, assets and CI pipeline are
ready in `packaging/aur/`; nothing is uploaded until an AUR account exists and
the repository secrets below are configured. AUR account registration is
temporarily closed, so the steps here wait until it reopens.

## What exists today

| Path | Purpose |
| --- | --- |
| `packaging/aur/PKGBUILD` | Source-built Arch recipe (pinned to `v0.1.0`) |
| `packaging/aur/.SRCINFO` | AUR metadata, generated with `makepkg --printsrcinfo` |
| `packaging/aur/ampwire.desktop` | Desktop launcher (`Terminal=true`) |
| `packaging/aur/ampwire.svg` | hicolor scalable icon (Winamp-homage bolt) |
| `.github/workflows/aur.yml` | Validate on every change, publish on demand |

The package installs: `/usr/bin/ampwire`, `/usr/share/applications/ampwire.desktop`,
`/usr/share/icons/hicolor/scalable/apps/ampwire.svg`,
`/usr/share/licenses/ampwire/LICENSE`, `/usr/share/doc/ampwire/README.md`.
Runtime dependencies are `alsa-lib` and `gcc-libs`; `rust` is a build-only
dependency. The build is fully offline after `cargo fetch --locked`
(`Cargo.lock` is committed).

## One-time setup (when AUR registration reopens)

1. Create an account at <https://aur.archlinux.org> and enable 2FA.
2. Generate a dedicated keypair for CI:
   ```sh
   ssh-keygen -t ed25519 -C "ampwire-aur-ci" -f ampwire-aur -N ""
   ```
3. Add the **public** half (`ampwire-aur.pub`) to your AUR account profile
   (*My Account → SSH Keys*).
4. Verify access: `ssh -i ampwire-aur aur@aur.archlinux.org` should answer
   `Hi <username>! You've successfully authenticated`.
5. Add two GitHub repository secrets on `3zcurdia/ampwire`:
   - `AUR_SSH_PRIVATE_KEY` — the full private key file contents.
   - `AUR_USERNAME` — your AUR username (used for the git author identity).

## Publishing

1. Make sure the **AUR** workflow's `validate` job is green on `main`.
2. GitHub → *Actions* → **AUR** → *Run workflow* → tick **publish**.
   The job clones `ssh://aur@aur.archlinux.org/ampwire.git` (an empty clone the
   first time creates the package), copies `packaging/aur/{PKGBUILD,ampwire.desktop,ampwire.svg}`,
   regenerates `.SRCINFO`, commits and pushes to `master`.
3. Check <https://aur.archlinux.org/packages/ampwire>: the page appears after
   the first push. From then on users install with `paru -S ampwire` / `yay -S ampwire`
   (or on Omarchy: *Install → AUR* / `omarchy pkg add ampwire`).
4. Once the listing is live, drop the "not yet listed" caveat from
   `README.md` and `docs/install.html` (the *Arch Linux (AUR)* sections).

## Bumping the version

1. Release the new tag (see `docs/RELEASING.md`).
2. Update `packaging/aur/PKGBUILD`: `pkgver`, `pkgrel=1`, and the tarball
   `sha256sums` entry:
   ```sh
   curl -sL https://github.com/3zcurdia/ampwire/archive/refs/tags/v$pkgver.tar.gz | sha256sum
   ```
3. Regenerate the metadata and sanity-check locally:
   ```sh
   cd packaging/aur
   makepkg --printsrcinfo > .SRCINFO
   makepkg -s            # full local build + tests
   ```
4. Commit, then publish with the workflow as above.

## Known constraint

`v0.1.0` (the currently pinned tag) predates MPRIS, the control socket and
`--status`/`--ctl` — those landed in `src/mpris.rs`, `src/control.rs` and
`src/remote.rs` after the tag. The Omarchy bar-widget plugin therefore
requires **≥ 0.2.0**; cut that release from `main` before the plugin ships,
then bump the PKGBUILD as described above.

## Build quirk: makepkg's `lto` option vs `ring`

Arch's `makepkg.conf` ships `OPTIONS=(… lto)` and `LTOFLAGS="-flto=auto"`,
and makepkg appends that to `CFLAGS`/`CXXFLAGS` right before `build()` runs
(`prepare_buildenv`, after `prepare()`). That compiles `ring`'s C shims as
GCC **slim LTO** objects: they carry no machine code and no usable symbol
map, so once `rustc` bundles them into the crate rlib, `rust-lld` fails with
`undefined symbol: ring_core_*`. The PKGBUILD's `_strip_lto` drops `-flto`
from `CFLAGS`/`CXXFLAGS` inside `build()`/`check()` to work around this. If
you see those errors again (e.g. after a makepkg change), check that the
strip still runs after makepkg's environment setup.

## Local verification commands

```sh
cd packaging/aur
bash -n PKGBUILD
shellcheck -e SC2034,SC2154 PKGBUILD   # if shellcheck is installed
makepkg --printsrcinfo | diff - .SRCINFO -
makepkg -s                              # builds, runs cargo test, packages
namcap PKGBUILD ./*.pkg.tar.zst         # if namcap is installed
```

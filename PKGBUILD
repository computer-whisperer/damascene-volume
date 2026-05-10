# Maintainer: Christian Balcom <robot.inventor@gmail.com>
#
# In-tree PKGBUILD: builds the working copy in place. Run `makepkg -si` from
# this directory to install the current source. AUR-quality packaging would
# fetch a tagged release tarball instead of consuming $startdir.

pkgname=aetna-volume
pkgver=0.2.0
pkgrel=1
pkgdesc='PipeWire volume control panel built with Aetna'
arch=('x86_64')
url='https://github.com/cbalcom/aetna-volume'
license=('MIT OR Apache-2.0')
depends=('libpipewire' 'libxkbcommon' 'vulkan-icd-loader' 'gcc-libs' 'glibc')
makedepends=('cargo' 'pkgconf')
# Disable system LTO — Arch's default `-flto=auto` lands in CFLAGS and makes
# libspa's C wrapper (compiled by its build.rs via the `cc` crate) emit
# LTO-IR objects, which rust-lld can't resolve at the final Rust link step.
options=('!lto')

build() {
    cd "$startdir"
    export CARGO_TARGET_DIR="$startdir/target"
    cargo build --release --locked --bin aetna-volume
}

package() {
    cd "$startdir"
    install -Dm755 "target/release/aetna-volume" "$pkgdir/usr/bin/aetna-volume"
    install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
    install -Dm644 aetna-volume.desktop \
        "$pkgdir/usr/share/applications/aetna-volume.desktop"
    # Scalable hicolor icon — `Icon=aetna-volume` in the .desktop entry
    # resolves here. Renaming on install so the icon name is stable
    # regardless of the source filename.
    install -Dm644 icon.svg \
        "$pkgdir/usr/share/icons/hicolor/scalable/apps/aetna-volume.svg"
}

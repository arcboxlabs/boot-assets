# ArcBox Boot Assets

`boot-assets` is the single source of truth for ArcBox VM boot artifacts.

Each release publishes per-architecture tarballs plus a unified multi-target manifest:

1. `boot-assets-{arch}-v{version}.tar.gz`
2. `boot-assets-{arch}-v{version}.tar.gz.sha256`
3. `manifest.json` (multi-target)

The tarball contains:

1. `kernel` — pre-built Linux kernel from [`arcboxlabs/kernel`](https://github.com/arcboxlabs/kernel) (all drivers built-in, `CONFIG_MODULES=n`)
2. `rootfs.erofs` — minimal read-only rootfs (busybox + mkfs.btrfs + iptables-legacy + ebtables + ethtool + socat + CA certs + optional FEX binfmt registration)
3. `manifest.json` — per-arch manifest (merged into unified manifest at release time)

No agent binary in the boot tarball, and no initramfs.
Guest runtime binaries are published separately as manifest-listed host-side
binaries and are shared into the VM via VirtioFS from the host.

## Manifest Schema

`schema_version` equals the major component of `asset_version` (e.g. `0.2.0` → `0`, `1.0.0` → `1`).

The manifest supports multiple target architectures and host-side binaries:

```jsonc
{
  "schema_version": 0,
  "asset_version": "0.2.0",
  "built_at": "2026-03-03T12:00:00Z",
  "source_repo": "arcboxlabs/kernel",
  "source_ref": "v0.1.0",
  "source_sha": "abc123",
  "targets": {
    "arm64": {
      "kernel": { "path": "arm64/kernel", "sha256": "...", "version": "6.12.8" },
      "rootfs": { "path": "arm64/rootfs.erofs", "sha256": "..." },
      "kernel_cmdline": "console=hvc0 root=/dev/vda ro rootfstype=erofs earlycon"
    },
    "x86_64": {
      "kernel": { "path": "x86_64/kernel", "sha256": "...", "version": "6.12.8" },
      "rootfs": { "path": "x86_64/rootfs.erofs", "sha256": "..." },
      "kernel_cmdline": "console=ttyS0 root=/dev/vda ro rootfstype=erofs earlycon"
    }
  },
  "binaries": [
    {
      "name": "dockerd",
      "version": "27.5.1",
      "targets": {
        "arm64":  { "path": "bin/arm64/dockerd",  "sha256": "..." },
        "x86_64": { "path": "bin/x86_64/dockerd", "sha256": "..." }
      }
    }
  ]
}
```

`boot-assets sync-binaries` supports both tarball extraction and direct binary
downloads. Use `format = "tgz"` plus `extract = "path/in/archive"` for archive
sources and `format = "binary"` for direct executable URLs.

## CLI Usage

The tool is built with Rust. Install with `cargo build --release`.

```bash
# Build EROFS rootfs only
boot-assets build rootfs --output build/rootfs.erofs --arch arm64

# Full release build (single arch)
boot-assets build release \
  --version 0.2.0 \
  --kernel build/kernel-arm64 \
  --arch arm64

# With pre-built rootfs
boot-assets build release \
  --version 0.2.0 \
  --kernel build/kernel-arm64 \
  --rootfs build/rootfs.erofs \
  --arch arm64 \
  --source-repo arcboxlabs/kernel \
  --source-ref v0.1.0

# Merge per-arch manifests into unified multi-target manifest
boot-assets merge-manifest dist/arm64/manifest.json dist/x86_64/manifest.json \
  --output dist/manifest.json
```

## Build And Release

### Verification

Local checks use the same commands as CI:

```bash
cargo fmt --check
cargo clippy --all-features -- -D warnings
cargo test --all-features --lib --bins
cargo build --features build --no-default-features
cargo test --features build --no-default-features --test e2e -- --nocapture
```

The Rust E2E target in `tests/e2e/` is organized as one Cargo integration test
crate with submodules. The default local E2E does not require a hypervisor. It
validates the published artifact contract: per-arch release tarballs contain the
expected files and checksums, the unified manifest points at the expected CDN
object paths with matching SHA256 values, and `AssetManager` can consume the
published layout over a real local HTTP server by downloading kernel/rootfs plus
runtime binaries into their install locations. This gives a Linux-compatible
product smoke test before higher-level VM/HV validation.

To validate a real published environment, run the ignored live E2E test against
the CDN. Omit `BOOT_ASSETS_LIVE_VERSION` to resolve `latest.json`:

```bash
BOOT_ASSETS_LIVE_CDN_BASE_URL=https://boot.arcboxcdn.com \
BOOT_ASSETS_LIVE_VERSION=0.5.1 \
BOOT_ASSETS_LIVE_ARCH=x86_64 \
BOOT_ASSETS_LIVE_PREPARE_BINARIES=true \
cargo test --features download --no-default-features --test e2e live::live_published_boot_assets_are_consumable -- --ignored --nocapture
```

To prove the kernel and rootfs boot as a real Linux system, run the QEMU boot
E2E test. This currently targets x86_64 and waits for the guest to reach Linux
userspace (`/sbin/init`) from the EROFS rootfs. Prefer passing local artifacts
from the current build:

```bash
# Requires qemu-system-x86_64 on PATH.
BOOT_ASSETS_RELEASE_TARBALL=dist/x86_64/boot-assets-x86_64-v0.5.1.tar.gz \
cargo test --features download --no-default-features --test e2e boot::qemu_boots_x86_64_linux -- --ignored --nocapture
```

You can also pass unpacked local artifacts directly:

```bash
BOOT_ASSETS_BOOT_KERNEL=dist/x86_64/kernel \
BOOT_ASSETS_BOOT_ROOTFS=dist/x86_64/rootfs.erofs \
BOOT_ASSETS_BOOT_CMDLINE="console=ttyS0 root=/dev/vda ro rootfstype=erofs earlycon" \
cargo test --features download --no-default-features --test e2e boot::qemu_boots_x86_64_linux -- --ignored --nocapture
```

If no local artifacts are provided, the same test falls back to the live CDN:

```bash
BOOT_ASSETS_LIVE_CDN_BASE_URL=https://boot.arcboxcdn.com \
BOOT_ASSETS_LIVE_VERSION=0.5.1 \
BOOT_ASSETS_LIVE_ARCH=x86_64 \
cargo test --features download --no-default-features --test e2e boot::qemu_boots_x86_64_linux -- --ignored --nocapture
```

GitHub Actions also exposes these as the manual `Live E2E` workflow for testing
actual published boot-assets on Linux, including an optional QEMU boot step.

### CI release workflow

Workflow file: `.github/workflows/release.yml`

Trigger:

1. Push tag: `v*`
2. Manual dispatch with explicit version

Pipeline stages:

1. **Download kernel** — downloads pre-built ARM64/x86_64 kernels from [`arcboxlabs/kernel`](https://github.com/arcboxlabs/kernel) release
2. **Build EROFS rootfs** — creates minimal rootfs from Alpine static binaries (per-arch)
3. **Assemble** — packages kernel + rootfs.erofs + manifest.json into tarball (per-arch)
4. **Merge** — merges per-arch manifests into unified multi-target manifest
5. **Release** — publishes to GitHub Releases and Backblaze B2 (fronted by Cloudflare at boot.arcboxcdn.com)

### Local build

Prerequisites:

1. Rust toolchain
2. Docker (for extracting static Alpine binaries and building the EROFS image)
3. Kernel binary from [`arcboxlabs/kernel`](https://github.com/arcboxlabs/kernel) release

```bash
# Build the CLI
cargo build --release

# Download kernel from arcboxlabs/kernel release
gh release download v0.1.0 --repo arcboxlabs/kernel --pattern "kernel-arm64" --dir build/

# Full release build
./target/release/boot-assets build release \
  --version 0.2.0 \
  --kernel build/kernel-arm64 \
  --arch arm64
```

Output files are written to `dist/`.

## EROFS Rootfs Contents

```
/ (EROFS, read-only, LZ4HC compressed)
├── bin/
│   └── busybox          # Static busybox (+ symlinks: sh, mount, mkdir, ...)
├── sbin/
│   ├── init             # Trampoline: mount /proc /sys /dev → mount VirtioFS → register FEX if available → exec agent
│   ├── mkfs.btrfs       # Btrfs formatter (first-boot data disk)
│   ├── iptables         # iptables-legacy (Docker bridge networking)
│   ├── ebtables         # bridge filter utility used by K3s
│   ├── ethtool          # network utility used by K3s
│   ├── socat            # stream relay utility used by K3s
│   └── (symlinks)       # iptables-save, iptables-restore, ip6tables, ...
├── lib/
│   └── *.so*            # musl loader + shared libs for packaged host utilities
├── cacerts/
│   └── ca-certificates.crt
└── (mount points)       # tmp/ run/ proc/ sys/ dev/ mnt/ arcbox/ Users/ etc/ var/
```

## FEX binfmt hook

The rootfs does not embed FEX itself. During boot, `/sbin/init` mounts the
`arcbox` VirtioFS share and checks for `/arcbox/runtime/bin/FEX` (the
`ARCBOX_RUNTIME_BIN_DIR` location ArcBox installs runtime binaries into,
alongside `dockerd`/`containerd`). If present, it mounts `binfmt_misc` and
registers the upstream FEX x86_64 ELF handler with `POCF` flags:

```text
:FEX-x86_64:M:0:\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00:\xff\xff\xff\xff\xff\xfe\xfe\x00\x00\x00\x00\xff\xff\xff\xff\xff\xfe\xff\xff\xff:/arcbox/runtime/bin/FEX:POCF
```

The registered interpreter is the arm64 FEX binary itself. `FEX_ROOTFS` is not
set; FEX's built-in default RootFS is `/`, so amd64 OCI containers provide their
own amd64 rootfs, loader, and shared libraries. The `F` flag pins the opened
interpreter at registration time so
x86_64 container processes can still invoke it even when `/arcbox` is not
visible inside the container rootfs. If FEX is absent, boot continues normally
and no x86_64 handler is registered.

FEX is built from source in the release workflow with `boot-assets build fex`.
The command builds the arm64 `FEX` interpreter as a
**static-pie** executable (`-static-pie`) and stages it into the binary
manifest — no dynamic library closure, and no `FEXServer`. Static linking is
required: the `F` flag pins only the interpreter executable's fd into the
container namespace, so a *dynamic* FEX would have the kernel resolve its
`PT_INTERP` against the container's amd64 rootfs (which lacks FEX's arm64
loader) and fail with `ENOENT`. The build asserts the produced binary carries no
`PT_INTERP` (`assert_static_executable`).

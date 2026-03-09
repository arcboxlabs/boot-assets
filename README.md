# ArcBox Boot Assets

`boot-assets` is the single source of truth for ArcBox VM boot artifacts.

Each release publishes per-architecture tarballs plus a unified multi-target manifest:

1. `boot-assets-{arch}-v{version}.tar.gz`
2. `boot-assets-{arch}-v{version}.tar.gz.sha256`
3. `manifest.json` (multi-target)

The tarball contains:

1. `kernel` — pre-built Linux kernel from [`arcboxlabs/kernel`](https://github.com/arcboxlabs/kernel) (all drivers built-in, `CONFIG_MODULES=n`)
2. `rootfs.erofs` — minimal read-only rootfs (busybox + mkfs.btrfs + iptables-legacy + ebtables + ethtool + socat + CA certs)
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
boot-assets build-rootfs --output build/rootfs.erofs --arch arm64

# Full release build (single arch)
boot-assets build-release \
  --version 0.2.0 \
  --kernel build/kernel-arm64 \
  --arch arm64

# With pre-built rootfs
boot-assets build-release \
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
5. **Release** — publishes to GitHub Releases and Cloudflare R2

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
./target/release/boot-assets build-release \
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
│   ├── init             # Trampoline: mount /proc /sys /dev → mount VirtioFS → exec agent
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

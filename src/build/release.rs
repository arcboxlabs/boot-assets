use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use fs_err as fs;
use humansize::{BINARY, format_size};

use crate::build::rootfs::{BuildRootfsOpts, build_rootfs};
use crate::build::runtime_image::build_runtime_image;
use arcbox_boot::manifest::{Binary, FileEntry, Manifest, Target, schema_version_for};
use arcbox_boot::util::{asset_object_path, create_tar_gz, read_json_file, sha256_file};

#[derive(Debug, Clone)]
pub struct BuildReleaseOpts {
    pub version: String,
    pub arch: String,
    pub kernel_path: PathBuf,
    pub rootfs_erofs_path: Option<PathBuf>,
    pub output_dir: PathBuf,
    pub erofs_compression: String,
    pub source_repo: Option<String>,
    pub source_ref: Option<String>,
    pub source_sha: Option<String>,
    pub kernel_version: Option<String>,
    /// Optional path to a JSON file containing `Vec<Binary>` entries
    /// (output of `sync-binaries`). Populates the manifest `binaries` field.
    pub binaries_json: Option<PathBuf>,
    /// Optional directory of guest runtime binaries (the local copies
    /// `sync-binaries` leaves behind). When set they are packed into a
    /// read-only `runtime.erofs` that the VM attaches as a block device, and
    /// the manifest target gains a `runtime` entry. When unset the target
    /// carries no `runtime` and consumers keep exec'ing over VirtioFS.
    pub runtime_bin_dir: Option<PathBuf>,
}

pub fn build_release(opts: &BuildReleaseOpts) -> Result<()> {
    if !opts.kernel_path.is_file() {
        bail!("kernel not found: {}", opts.kernel_path.display());
    }

    let work_dir = tempfile::tempdir().context("failed to create work dir")?;
    let work = work_dir.path();
    fs::create_dir_all(&opts.output_dir)?;

    // Step 1: Build or copy EROFS rootfs.
    let rootfs_work = work.join("rootfs.erofs");
    if let Some(ref rootfs_path) = opts.rootfs_erofs_path {
        if !rootfs_path.is_file() {
            bail!("rootfs.erofs not found: {}", rootfs_path.display());
        }
        println!(
            "==> Using pre-built rootfs.erofs: {}",
            rootfs_path.display()
        );
        fs::copy(rootfs_path, &rootfs_work)?;
    } else {
        println!("==> Building EROFS rootfs");
        build_rootfs(&BuildRootfsOpts {
            output: rootfs_work.clone(),
            arch: opts.arch.clone(),
            compression: opts.erofs_compression.clone(),
        })?;
    }

    // Step 2: Copy kernel.
    println!("==> Copying kernel");
    let kernel_work = work.join("kernel");
    fs::copy(&opts.kernel_path, &kernel_work)?;

    // Step 3: Generate manifest.
    let kernel_sha256 = sha256_file(&kernel_work)?;
    let rootfs_sha256 = sha256_file(&rootfs_work)?;
    let built_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let kernel_cmdline = match opts.arch.as_str() {
        "arm64" => "console=hvc0 root=/dev/vda ro rootfstype=erofs earlycon",
        "x86_64" => "console=ttyS0 root=/dev/vda ro rootfstype=erofs earlycon",
        _ => "console=hvc0 root=/dev/vda ro rootfstype=erofs earlycon",
    };

    // Optional: pack the guest runtime binaries into a read-only image.
    let runtime_entry = if let Some(ref bin_dir) = opts.runtime_bin_dir {
        if !bin_dir.is_dir() {
            bail!("runtime bin dir not found: {}", bin_dir.display());
        }
        let runtime_work = work.join("runtime.erofs");
        build_runtime_image(bin_dir, &runtime_work, &opts.erofs_compression)?;
        Some(FileEntry {
            path: asset_object_path(&opts.version, &opts.arch, "runtime.erofs"),
            sha256: sha256_file(&runtime_work)?,
            version: None,
        })
    } else {
        None
    };

    let runtime_built = runtime_entry.is_some();

    let target = Target {
        kernel: FileEntry {
            path: asset_object_path(&opts.version, &opts.arch, "kernel"),
            sha256: kernel_sha256,
            version: opts.kernel_version.clone(),
        },
        rootfs: FileEntry {
            path: asset_object_path(&opts.version, &opts.arch, "rootfs.erofs"),
            sha256: rootfs_sha256,
            version: None,
        },
        kernel_cmdline: kernel_cmdline.to_string(),
        runtime: runtime_entry,
    };

    let mut targets = BTreeMap::new();
    targets.insert(opts.arch.clone(), target);

    let schema_version = schema_version_for(&opts.version);

    let manifest = Manifest {
        schema_version,
        asset_version: opts.version.clone(),
        built_at,
        source_repo: opts.source_repo.clone(),
        source_ref: opts.source_ref.clone(),
        source_sha: opts.source_sha.clone(),
        targets,
        binaries: load_binaries_json(&opts.binaries_json)?,
    };

    println!("==> Generating manifest.json (schema v{schema_version})");
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    let manifest_work = work.join("manifest.json");
    fs::write(&manifest_work, &manifest_json)?;

    // Step 4: Package tarball.
    let tarball_name = format!("boot-assets-{}-v{}.tar.gz", opts.arch, opts.version);
    let tarball_path = opts.output_dir.join(&tarball_name);

    println!("==> Packaging tarball");
    let mut tarball_files = vec!["kernel", "rootfs.erofs", "manifest.json"];
    if runtime_built {
        tarball_files.push("runtime.erofs");
    }
    create_tar_gz(&tarball_path, work, &tarball_files)?;

    // Write checksum.
    let tarball_sha = sha256_file(&tarball_path)?;
    fs::write(
        opts.output_dir.join(format!("{tarball_name}.sha256")),
        format!("{tarball_sha}  {tarball_name}\n"),
    )?;

    // Copy manifest to output dir.
    fs::write(opts.output_dir.join("manifest.json"), &manifest_json)?;

    // The runtime image is uploaded to the CDN next to the kernel/rootfs, so
    // it has to leave the temp work dir too.
    if runtime_built {
        fs::copy(
            work.join("runtime.erofs"),
            opts.output_dir.join("runtime.erofs"),
        )?;
    }

    let tarball_size = format_size(fs::metadata(&tarball_path)?.len(), BINARY);
    let kernel_size = format_size(fs::metadata(&kernel_work)?.len(), BINARY);
    let rootfs_size = format_size(fs::metadata(&rootfs_work)?.len(), BINARY);

    println!();
    println!("========================================");
    println!("  Boot Assets v{} (schema v{schema_version})", opts.version);
    println!("========================================");
    println!();
    println!("  Tarball:  {} ({tarball_size})", tarball_path.display());
    println!("  Kernel:   {kernel_size}");
    println!(
        "  Rootfs:   {rootfs_size} (EROFS, {})",
        opts.erofs_compression
    );
    println!("  Manifest: schema_version {schema_version}");
    println!();
    println!(
        "  Checksum: {}",
        opts.output_dir
            .join(format!("{tarball_name}.sha256"))
            .display()
    );
    println!(
        "  Manifest: {}",
        opts.output_dir.join("manifest.json").display()
    );
    println!();

    Ok(())
}

/// Merge a single-arch manifest into an existing multi-arch manifest.
///
/// Used by CI: each arch job produces a single-target manifest, then a final
/// step merges them into one unified manifest.
pub fn merge_manifests(base: &mut Manifest, other: &Manifest) -> Result<()> {
    if base.asset_version != other.asset_version {
        bail!(
            "cannot merge manifests with different versions: {} vs {}",
            base.asset_version,
            other.asset_version
        );
    }
    for (arch, target) in &other.targets {
        base.targets.insert(arch.clone(), target.clone());
    }
    for bin in &other.binaries {
        if let Some(existing) = base.binaries.iter_mut().find(|b| b.name == bin.name) {
            for (arch, bt) in &bin.targets {
                existing.targets.insert(arch.clone(), bt.clone());
            }
        } else {
            base.binaries.push(bin.clone());
        }
    }
    Ok(())
}

fn load_binaries_json(path: &Option<PathBuf>) -> Result<Vec<Binary>> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    read_json_file(path)
        .with_context(|| format!("failed to parse binaries JSON from {}", path.display()))
}

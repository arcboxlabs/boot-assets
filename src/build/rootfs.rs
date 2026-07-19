use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs_err as fs;
use humansize::{BINARY, format_size};
use minijinja::context;
use xshell::{Shell, cmd};

use arcbox_boot::util::{copy_executable, render_template, set_executable};

const BUSYBOX_SYMLINKS: &[&str] = &[
    "sh", "mount", "umount", "mkdir", "cat", "echo", "sleep", "ln", "chmod", "chown", "cp", "mv",
    "rm", "ls", "ip", "hostname", "sysctl",
];

const IPTABLES_SYMLINKS: &[&str] = &[
    "iptables-save",
    "iptables-restore",
    "ip6tables",
    "ip6tables-save",
    "ip6tables-restore",
];

/// Core statically-linked binaries built from source inside Docker.
///
/// `mkfs.erofs` is a runtime dependency of containerd's erofs snapshotter
/// (its differ converts layer tars into EROFS blobs); the Alpine package is
/// too old (containerd prefers erofs-utils >= 1.8.2), so it is built from
/// source like the other core tools.
const CORE_STATIC_BINARIES: &[&str] = &["busybox", "mkfs.btrfs", "iptables", "mkfs.erofs"];

/// NFS server utilities (Alpine `nfs-utils` package).
const NFS_PACKAGES: &[&str] = &["nfs-utils"];
const NFS_BINARIES: &[&str] = &["rpc.nfsd", "exportfs", "rpc.mountd"];

const EROFS_BLOCK_SIZE: &str = "4096";
const EROFS_XATTR_TOLERANCE: &str = "-1";

const MOUNT_DIRS: &[&str] = &[
    "tmp", "run", "proc", "sys", "dev", "mnt", "arcbox", "Users", "etc", "var", "export",
];

// FEX is delivered as an ArcBox runtime binary, so it lands in the guest under
// `/arcbox/runtime/bin/` (the `ARCBOX_RUNTIME_BIN_DIR` convention shared with
// `dockerd`/`containerd`), alongside the VirtioFS `arcbox` share root.
const FEX_BINARY: &str = "/arcbox/runtime/bin/FEX";

const FEX_X86_64_BINFMT_ENTRY: &str = r#":FEX-x86_64:M:0:\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00:\xff\xff\xff\xff\xff\xfe\xfe\x00\x00\x00\x00\xff\xff\xff\xff\xff\xfe\xff\xff\xff:/arcbox/runtime/bin/FEX:POCF"#;

/// Path to the staged guest agent on the VirtioFS `arcbox` share.
const AGENT_BIN: &str = "/arcbox/bin/arcbox-agent";

/// busybox `inittab` driving PID 1.
///
/// The kernel runs `/sbin/init` (a symlink to the busybox multi-call binary), so
/// busybox `init` is PID 1. It parses this table once at boot, then:
///   1. runs the `sysinit` entry (rcS: early mounts + one-shot `arcbox-agent
///      init`) to completion, then
///   2. `respawn`s the long-running agent — restarting it if it ever exits, so a
///      crashing agent no longer panics the kernel as a dead PID 1.
///
/// Ctrl-Alt-Del maps to an orderly poweroff.
fn inittab() -> String {
    format!(
        "::sysinit:/etc/init.d/rcS\n\
         ::respawn:{AGENT_BIN}\n\
         ::ctrlaltdel:/bin/busybox poweroff\n"
    )
}

/// `sysinit` script run once by busybox init before the agent is respawned.
///
/// Mounts the early pseudo-filesystems and the VirtioFS `arcbox` share, registers
/// FEX for amd64 ELF binaries, then runs the agent's one-shot system
/// initialization (`arcbox-agent init`: writable mounts, networking, `/etc`). On
/// success `init` exits 0 and busybox respawns the long-running agent. A non-zero
/// exit means a critical writable layer failed to mount, so rcS powers the VM off
/// for a clean host-driven retry instead of respawning an agent that would run
/// broken on the read-only EROFS rootfs.
fn rcs_script() -> Result<String> {
    render_template(
        "rcS.sh",
        include_str!("scripts/rcS.sh"),
        context! {
            FEX_BINARY => FEX_BINARY,
            FEX_X86_64_BINFMT_ENTRY => FEX_X86_64_BINFMT_ENTRY,
            AGENT_BIN => AGENT_BIN,
        },
    )
}

/// Total number of binary build steps.
fn total_build_steps() -> usize {
    CORE_STATIC_BINARIES.len() + NFS_BINARIES.len()
}

fn nfs_apk_packages() -> String {
    NFS_PACKAGES.join(" ")
}

fn nfs_stage_script() -> Result<String> {
    let start_index = CORE_STATIC_BINARIES.len() + 1;
    let end_index = start_index + NFS_BINARIES.len() - 1;
    let total = total_build_steps();
    let case_arms = indexed_case_arms(NFS_BINARIES, start_index);
    render_template(
        "stage-nfs-utilities.sh",
        include_str!("scripts/stage-nfs-utilities.sh"),
        context! {
            start_index,
            end_index,
            nfs_binaries => NFS_BINARIES.join(" "),
            case_arms,
            total,
        },
    )
}

fn indexed_case_arms(binaries: &[&str], start_index: usize) -> String {
    binaries
        .iter()
        .enumerate()
        .map(|(offset, binary)| format!("    {binary}) idx={} ;;\n", start_index + offset))
        .collect()
}

fn nfs_out_paths() -> String {
    NFS_BINARIES
        .iter()
        .map(|binary| format!("/out/{binary}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone)]
pub struct BuildRootfsOpts {
    pub output: PathBuf,
    pub arch: String,
    pub compression: String,
}

pub fn build_rootfs(opts: &BuildRootfsOpts) -> Result<()> {
    let (docker_platform, _alpine_arch) = match opts.arch.as_str() {
        "arm64" => ("linux/arm64", "aarch64"),
        "x86_64" => ("linux/amd64", "x86_64"),
        other => bail!("unsupported arch: {other}"),
    };

    let sh = Shell::new()?;
    let staging = tempfile::tempdir().context("failed to create temp dir")?;
    let staging_path = staging.path();
    let nfs_packages = nfs_apk_packages();
    let nfs_stage_script = nfs_stage_script()?;
    let nfs_out_paths = nfs_out_paths();
    let nfs_binaries_list = NFS_BINARIES.join(" ");
    let total = total_build_steps();

    // Step 1: Build core static binaries and stage packaged k3s host utilities.
    println!("==> Building rootfs binaries via Docker ({docker_platform})");
    let docker_script = render_template(
        "build-rootfs-binaries.sh",
        include_str!("scripts/build-rootfs-binaries.sh"),
        context! {
            nfs_packages,
            total,
            nfs_stage_script,
            nfs_out_paths,
            nfs_binaries_list,
        },
    )?;

    let out_mount = format!("{}:/out", staging_path.display());
    cmd!(
        sh,
        "docker run --rm --platform {docker_platform} -v {out_mount} alpine:3.19 sh -c {docker_script}"
    )
    .run()
    .context("docker static build failed")?;

    // Step 2: Build rootfs staging directory.
    println!("==> Building EROFS rootfs staging directory");
    let rootfs = staging_path.join("rootfs");
    build_rootfs_tree(&rootfs, staging_path)?;

    // Step 3: Create EROFS image.
    println!("==> Creating EROFS image");
    build_erofs_image_with_docker(
        &sh,
        docker_platform,
        &rootfs,
        &opts.output,
        &opts.compression,
    )?;

    let size = format_size(fs::metadata(&opts.output)?.len(), BINARY);
    println!();
    println!("==> EROFS rootfs built: {} ({size})", opts.output.display());
    println!("    Compression: {}", opts.compression);
    println!("    Block size: {} bytes", EROFS_BLOCK_SIZE);
    println!(
        "    Contents: busybox + mkfs.btrfs + iptables-legacy + mkfs.erofs + nfs-utils + CA certs + busybox-init boot sequence"
    );
    println!("    Core boot tools are static; packaged utilities include required shared libs");

    Ok(())
}

fn build_erofs_image_with_docker(
    sh: &Shell,
    docker_platform: &str,
    rootfs: &Path,
    output: &Path,
    compression: &str,
) -> Result<()> {
    let output_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid output filename: {}", output.display()))?;
    let output_dir = output
        .parent()
        .ok_or_else(|| anyhow::anyhow!("output path has no parent: {}", output.display()))?;
    fs::create_dir_all(output_dir)?;

    // Copy the host-assembled tree into a container-local overlay so we can create
    // the /dev/console and /dev/null device nodes busybox init expects at PID 1
    // startup. The bind-mounted /rootfs is macOS-backed and read-only and cannot
    // hold Linux device nodes, so they are created here in the Linux overlay and
    // then packed by mkfs.erofs (which sources /build, not /rootfs).
    let install_and_run = include_str!("scripts/mkfs-erofs.sh");

    let rootfs_mount = format!("{}:/rootfs:ro", rootfs.display());
    let output_mount = format!("{}:/out", output_dir.display());
    let block_flag = mkfs_erofs_block_flag();
    let xattr_flag = format!("-x{EROFS_XATTR_TOLERANCE}");
    let compression_flag = format!("-z{compression}");
    let output_path = format!("/out/{output_name}");
    cmd!(
        sh,
        "docker run --rm --platform {docker_platform} -v {rootfs_mount} -v {output_mount} alpine:3.19 sh -c {install_and_run} -- {block_flag} {xattr_flag} {compression_flag} {output_path} /build"
    )
    .run()
    .context("docker mkfs.erofs failed")?;

    Ok(())
}

fn build_rootfs_tree(rootfs: &Path, staging: &Path) -> Result<()> {
    // /bin — busybox + symlinks
    let bin_dir = rootfs.join("bin");
    fs::create_dir_all(&bin_dir)?;
    copy_executable(&staging.join("busybox"), &bin_dir.join("busybox"))?;
    for cmd in BUSYBOX_SYMLINKS {
        fs::os::unix::fs::symlink("busybox", bin_dir.join(cmd))?;
    }

    // /sbin — system binaries
    let sbin_dir = rootfs.join("sbin");
    fs::create_dir_all(&sbin_dir)?;
    copy_executable(&staging.join("mkfs.btrfs"), &sbin_dir.join("mkfs.btrfs"))?;
    copy_executable(&staging.join("iptables"), &sbin_dir.join("iptables"))?;
    copy_executable(&staging.join("mkfs.erofs"), &sbin_dir.join("mkfs.erofs"))?;
    for binary in NFS_BINARIES {
        copy_executable(&staging.join(binary), &sbin_dir.join(binary))?;
    }
    for link in IPTABLES_SYMLINKS {
        fs::os::unix::fs::symlink("iptables", sbin_dir.join(link))?;
    }

    // /lib — dynamic loader and shared libs for packaged utilities.
    let lib_dir = rootfs.join("lib");
    fs::create_dir_all(&lib_dir)?;
    let staged_lib_dir = staging.join("lib");
    if staged_lib_dir.is_dir() {
        for entry in fs::read_dir(staged_lib_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                fs::copy(&path, lib_dir.join(entry.file_name()))?;
            }
        }
    }

    // /cacerts
    let cacerts_dir = rootfs.join("cacerts");
    fs::create_dir_all(&cacerts_dir)?;
    fs::copy(
        staging.join("ca-certificates.crt"),
        cacerts_dir.join("ca-certificates.crt"),
    )?;

    // Mount point directories
    for dir in MOUNT_DIRS {
        fs::create_dir_all(rootfs.join(dir))?;
    }

    // /sbin/init symlink + /etc/inittab + /etc/init.d/rcS — the busybox init
    // boot sequence.
    write_boot_sequence(rootfs)?;

    Ok(())
}

/// Writes the busybox-init boot sequence into the rootfs tree: `/sbin/init` (a
/// symlink to the busybox multi-call binary, which dispatches on
/// `basename(argv[0]) == "init"`), `/etc/inittab`, and the one-shot
/// `/etc/init.d/rcS` sysinit script. busybox parses inittab once at boot, before
/// `arcbox-agent init` (invoked by rcS) later mounts a writable tmpfs over /etc,
/// so that shadowing is harmless. The `/dev/console` and `/dev/null` device nodes
/// busybox init also needs are created later in the Linux overlay during
/// mkfs.erofs (the macOS-backed staging tree can't hold device nodes).
fn write_boot_sequence(rootfs: &Path) -> Result<()> {
    let sbin_dir = rootfs.join("sbin");
    fs::create_dir_all(&sbin_dir)?;
    fs::os::unix::fs::symlink("/bin/busybox", sbin_dir.join("init"))?;

    let etc_dir = rootfs.join("etc");
    let init_d_dir = etc_dir.join("init.d");
    fs::create_dir_all(&init_d_dir)?;
    fs::write(etc_dir.join("inittab"), inittab())?;
    fs::write(init_d_dir.join("rcS"), rcs_script()?)?;
    set_executable(&init_d_dir.join("rcS"))?;

    // Machine boot shim: PID 1 for distro machines, selected by the host via
    // `init=/sbin/arcbox-machine-init` (see the script header for the
    // contract). Inert on the System VM path, which boots busybox init.
    fs::write(
        sbin_dir.join("arcbox-machine-init"),
        include_str!("scripts/machine-init.sh"),
    )?;
    set_executable(&sbin_dir.join("arcbox-machine-init"))?;
    Ok(())
}

fn mkfs_erofs_block_flag() -> String {
    format!("-b{EROFS_BLOCK_SIZE}")
}

#[cfg(test)]
mod tests {
    use fs_err as fs;

    use super::{
        FEX_BINARY, FEX_X86_64_BINFMT_ENTRY, inittab, mkfs_erofs_block_flag, rcs_script,
        write_boot_sequence,
    };

    #[test]
    fn mkfs_erofs_block_flag_uses_4k_syntax() {
        assert_eq!(mkfs_erofs_block_flag(), "-b4096");
    }

    #[test]
    fn erofs_xattr_tolerance_disables_xattrs() {
        assert_eq!(super::EROFS_XATTR_TOLERANCE, "-1");
    }

    #[test]
    fn fex_binfmt_entry_matches_upstream_shape() {
        assert!(FEX_X86_64_BINFMT_ENTRY.starts_with(":FEX-x86_64:M:0:"));
        assert!(FEX_X86_64_BINFMT_ENTRY.contains(r"\x7fELF\x02"));
        assert!(FEX_X86_64_BINFMT_ENTRY.contains(r"\x3e\x00"));
        assert!(FEX_X86_64_BINFMT_ENTRY.ends_with(&format!(":{FEX_BINARY}:POCF")));
        assert!(!FEX_X86_64_BINFMT_ENTRY.contains('\0'));
    }

    #[test]
    fn rcs_script_runs_agent_init_after_virtiofs_and_fex() {
        let script = rcs_script().unwrap();
        let mount_arcbox = script.find("mount -t virtiofs arcbox /arcbox").unwrap();
        let fex_check = script.find(&format!("[ -x {FEX_BINARY} ]")).unwrap();
        let agent_init = script.find("/arcbox/bin/arcbox-agent init").unwrap();

        // FEX registers after the share is mounted; the one-shot `arcbox-agent
        // init` runs last so busybox init can then respawn the long-running agent.
        assert!(mount_arcbox < fex_check);
        assert!(fex_check < agent_init);
        assert!(script.contains("mount -t binfmt_misc binfmt_misc"));
        assert!(script.contains(FEX_X86_64_BINFMT_ENTRY));
        // sysinit is one-shot: it must not exec/replace itself with the agent.
        assert!(!script.contains("exec /arcbox/bin/arcbox-agent"));
        // No busybox `timeout` wrapper: its arg syntax is version-dependent and a
        // misparse would be silently swallowed, skipping init.
        assert!(!script.contains("timeout"));
        // A non-zero init exit (critical mount failure) powers off for a clean
        // host-driven retry rather than swallowing the error and respawning a
        // broken agent.
        assert!(script.contains("poweroff -f"));
        assert!(!script.contains("init || true"));
        assert!(!script.contains("export FEX_ROOTFS"));
        assert!(!script.contains('\0'));
    }

    #[test]
    fn inittab_supervises_agent_as_pid1() {
        let tab = inittab();
        assert!(tab.contains("::sysinit:/etc/init.d/rcS"));
        // respawn => busybox init restarts the agent if it ever exits, so a crash
        // no longer kills PID 1.
        assert!(tab.contains("::respawn:/arcbox/bin/arcbox-agent"));
        assert!(tab.contains("::ctrlaltdel:/bin/busybox poweroff"));
        assert!(!tab.contains('\0'));
    }

    /// Assembles the boot sequence into a real temp tree and asserts the on-disk
    /// layout busybox init depends on (the device nodes are added later in the
    /// Docker overlay, so they are out of scope here).
    #[test]
    fn write_boot_sequence_assembles_busybox_init_tree() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("arcbox-boot-seq-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        write_boot_sequence(&root).unwrap();

        // /sbin/init is a symlink to the busybox multi-call binary (init applet).
        let init = root.join("sbin/init");
        assert_eq!(
            fs::read_link(&init).unwrap(),
            std::path::Path::new("/bin/busybox")
        );

        // /etc/inittab carries the supervision entries.
        let tab = fs::read_to_string(root.join("etc/inittab")).unwrap();
        assert!(tab.contains("::sysinit:/etc/init.d/rcS"));
        assert!(tab.contains("::respawn:/arcbox/bin/arcbox-agent"));

        // /etc/init.d/rcS is executable and runs the one-shot init after mounting.
        let rcs = root.join("etc/init.d/rcS");
        let body = fs::read_to_string(&rcs).unwrap();
        assert!(body.contains("mount -t virtiofs arcbox /arcbox"));
        assert!(body.contains("/arcbox/bin/arcbox-agent init"));
        let mode = fs::metadata(&rcs).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "rcS must be executable");

        // /sbin/arcbox-machine-init is executable and honors the machine
        // cmdline contract (keys owned by arcbox's arcbox-constants).
        let shim = root.join("sbin/arcbox-machine-init");
        let body = fs::read_to_string(&shim).unwrap();
        for needle in [
            "arcbox.machine_rootfs=",
            "arcbox.machine_rootfs_type=",
            "arcbox.machine_data=",
            "arcbox.machine_mounts=",
            "pivot_root",
            "mount -t virtiofs arcbox /arcbox",
            "/arcbox/bin/arcbox-agent machine-init",
            "/arcbox/bin/arcbox-agent serve",
        ] {
            assert!(body.contains(needle), "machine-init missing: {needle}");
        }
        let mode = fs::metadata(&shim).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "machine-init must be executable");

        fs::remove_dir_all(&root).ok();
    }
}

use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::support::fetch_latest_version;
use arcbox_boot::asset_manager::{AssetManager, AssetManagerConfig};
use arcbox_boot::manifest::Manifest;

const DEFAULT_X86_64_CMDLINE: &str = "console=ttyS0 root=/dev/vda ro rootfstype=erofs earlycon";

#[test]
#[ignore = "boots real boot-assets under QEMU"]
fn qemu_boots_x86_64_linux() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();

        if let Some(assets) = local_boot_assets(temp.path()) {
            boot_x86_64_linux(&assets);
            return;
        }

        let cdn_base_url = env::var("BOOT_ASSETS_LIVE_CDN_BASE_URL")
            .unwrap_or_else(|_| "https://boot.arcboxcdn.com".to_string());
        let version = match env::var("BOOT_ASSETS_LIVE_VERSION") {
            Ok(version) if !version.is_empty() => version,
            _ => fetch_latest_version(&cdn_base_url).await,
        };
        let arch = env::var("BOOT_ASSETS_LIVE_ARCH").unwrap_or_else(|_| "x86_64".to_string());
        assert_eq!(arch, "x86_64", "QEMU boot E2E currently supports x86_64");

        let manager = AssetManager::new(AssetManagerConfig {
            cdn_base_url,
            version,
            arch,
            cache_dir: temp.path().join("boot-cache"),
            custom_kernel: None,
        })
        .unwrap();
        let prepared = manager.prepare(None).await.unwrap();
        let assets = BootAssets {
            kernel: prepared.kernel,
            rootfs: prepared.rootfs,
            kernel_cmdline: prepared.kernel_cmdline,
        };

        boot_x86_64_linux(&assets);
    });
}

struct BootAssets {
    kernel: PathBuf,
    rootfs: PathBuf,
    kernel_cmdline: String,
}

fn local_boot_assets(work_dir: &Path) -> Option<BootAssets> {
    match (
        env::var_os("BOOT_ASSETS_BOOT_KERNEL"),
        env::var_os("BOOT_ASSETS_BOOT_ROOTFS"),
    ) {
        (Some(kernel), Some(rootfs)) => {
            return Some(BootAssets {
                kernel: PathBuf::from(kernel),
                rootfs: PathBuf::from(rootfs),
                kernel_cmdline: env::var("BOOT_ASSETS_BOOT_CMDLINE")
                    .unwrap_or_else(|_| DEFAULT_X86_64_CMDLINE.to_string()),
            });
        }
        (None, None) => {}
        _ => panic!("BOOT_ASSETS_BOOT_KERNEL and BOOT_ASSETS_BOOT_ROOTFS must be set together"),
    }

    env::var_os("BOOT_ASSETS_RELEASE_TARBALL")
        .map(PathBuf::from)
        .map(|tarball| extract_release_tarball(&tarball, &work_dir.join("release")))
}

fn extract_release_tarball(tarball: &Path, dest: &Path) -> BootAssets {
    fs::create_dir_all(dest).unwrap();
    let output = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(dest)
        .output()
        .unwrap_or_else(|error| panic!("failed to run tar for {}: {error}", tarball.display()));
    if !output.status.success() {
        panic!(
            "failed to extract {}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            tarball.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let manifest: Manifest = serde_json::from_slice(&fs::read(dest.join("manifest.json")).unwrap())
        .unwrap_or_else(|error| panic!("failed to parse extracted manifest: {error}"));
    let kernel_cmdline = manifest
        .targets
        .get("x86_64")
        .map(|target| target.kernel_cmdline.clone())
        .unwrap_or_else(|| DEFAULT_X86_64_CMDLINE.to_string());

    BootAssets {
        kernel: dest.join("kernel"),
        rootfs: dest.join("rootfs.erofs"),
        kernel_cmdline,
    }
}

fn boot_x86_64_linux(assets: &BootAssets) {
    let qemu = env::var("BOOT_ASSETS_QEMU_SYSTEM_X86_64")
        .unwrap_or_else(|_| "qemu-system-x86_64".to_string());
    let timeout = Duration::from_secs(
        env::var("BOOT_ASSETS_QEMU_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(90),
    );
    let cmdline = format!("{} panic=-1 printk.time=0", assets.kernel_cmdline.trim());
    let rootfs_drive = format!(
        "file={},format=raw,if=virtio,readonly=on",
        assets.rootfs.display()
    );

    let mut child = Command::new(&qemu)
        .args([
            "-nodefaults",
            "-machine",
            "q35,accel=tcg",
            "-cpu",
            "max",
            "-m",
            "512M",
            "-smp",
            "1",
            "-kernel",
        ])
        .arg(&assets.kernel)
        .args(["-drive", &rootfs_drive, "-append", &cmdline])
        .args(["-serial", "stdio", "-display", "none", "-no-reboot"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {qemu}: {error}"));

    let (tx, rx) = mpsc::channel();
    forward_lines(&mut child, OutputStream::Stdout, tx.clone());
    forward_lines(&mut child, OutputStream::Stderr, tx);

    let output = wait_for_linux_boot(&mut child, rx, timeout);
    child.kill().ok();
    child.wait().ok();

    assert!(
        output.contains("Run /sbin/init as init process")
            || output.contains("init started: BusyBox"),
        "QEMU did not reach Linux userspace before timeout. Output:\n{output}"
    );
}

#[derive(Debug, Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

fn forward_lines(child: &mut Child, stream: OutputStream, tx: mpsc::Sender<String>) {
    let reader: Box<dyn std::io::Read + Send> = match stream {
        OutputStream::Stdout => Box::new(child.stdout.take().unwrap()),
        OutputStream::Stderr => Box::new(child.stderr.take().unwrap()),
    };

    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    let _ = tx.send(line);
                }
                Err(_) => break,
            }
        }
    });
}

fn wait_for_linux_boot(child: &mut Child, rx: mpsc::Receiver<String>, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut output = String::new();

    loop {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("QEMU exited before Linux userspace booted: {status}\n{output}");
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return output;
        }

        match rx.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(line) => {
                output.push_str(&line);
                output.push('\n');
                if line.contains("Run /sbin/init as init process")
                    || line.contains("init started: BusyBox")
                {
                    return output;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return output,
        }
    }
}

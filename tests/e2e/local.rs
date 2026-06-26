use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use arcbox_boot::asset_manager::{AssetManager, AssetManagerConfig};
use arcbox_boot::manifest::{Binary, BinaryTarget, Manifest};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;

const VERSION: &str = "9.9.9-e2e";

#[test]
fn published_boot_assets_are_consumable_without_hv() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = PublishedFixture::build(temp.path());

    assert_release_tarball_contract(&fixture.release_dir("x86_64"), "x86_64");
    assert_release_tarball_contract(&fixture.release_dir("arm64"), "arm64");
    assert_manifest_contract(
        &fixture
            .cdn_root
            .join(format!("asset/v{VERSION}/manifest.json")),
    );

    let server = HttpServer::spawn(fixture.cdn_root.clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let cache_dir = temp.path().join("cache");
        let manager = AssetManager::new(AssetManagerConfig {
            cdn_base_url: server.base_url(),
            version: VERSION.to_string(),
            arch: "x86_64".to_string(),
            cache_dir: cache_dir.clone(),
            custom_kernel: None,
        })
        .unwrap();

        let prepared = manager.prepare(None).await.unwrap();
        assert_eq!(prepared.version, VERSION);
        assert_eq!(
            prepared.kernel_cmdline,
            "console=ttyS0 root=/dev/vda ro rootfstype=erofs earlycon"
        );
        assert_eq!(
            fs::read_to_string(&prepared.kernel).unwrap(),
            "fake-kernel-x86_64\n"
        );
        assert_eq!(
            fs::read_to_string(&prepared.rootfs).unwrap(),
            "fake-rootfs-x86_64\n"
        );

        let bin_dir = temp.path().join("guest/bin");
        manager.prepare_binaries(&bin_dir, None).await.unwrap();

        let default_binary = bin_dir.join("direct-tool");
        assert_eq!(
            fs::read_to_string(&default_binary).unwrap(),
            "#!/bin/sh\necho direct-tool\n"
        );
        assert_executable(&default_binary);

        let runtime_binary = temp.path().join("guest/runtime/bin/runtime-tool");
        assert_eq!(
            fs::read_to_string(&runtime_binary).unwrap(),
            "#!/bin/sh\necho runtime-tool\n"
        );
        assert_executable(&runtime_binary);

        prepared
            .manifest
            .validate_binaries("x86_64", &bin_dir)
            .await
            .unwrap();

        assert!(cache_dir.join(VERSION).join("manifest.json").is_file());
        assert!(cache_dir.join(VERSION).join("kernel").is_file());
        assert!(cache_dir.join(VERSION).join("rootfs.erofs").is_file());
    });
}

struct PublishedFixture {
    output: PathBuf,
    cdn_root: PathBuf,
}

impl PublishedFixture {
    fn build(root: &Path) -> Self {
        let output = root.join("release");
        let cdn_root = root.join("cdn");
        fs::create_dir_all(&output).unwrap();
        fs::create_dir_all(&cdn_root).unwrap();

        let binaries_json = root.join("binaries-x86_64.json");
        let binaries = write_runtime_binaries(&cdn_root);
        fs::write(
            &binaries_json,
            serde_json::to_string_pretty(&binaries).unwrap(),
        )
        .unwrap();

        let empty_binaries_json = root.join("empty-binaries.json");
        fs::write(&empty_binaries_json, "[]\n").unwrap();

        for arch in ["x86_64", "arm64"] {
            fs::write(
                root.join(format!("kernel-{arch}")),
                format!("fake-kernel-{arch}\n"),
            )
            .unwrap();
            fs::write(
                root.join(format!("rootfs-{arch}.erofs")),
                format!("fake-rootfs-{arch}\n"),
            )
            .unwrap();

            run_boot_assets([
                "build",
                "release",
                "--version",
                VERSION,
                "--kernel",
                root.join(format!("kernel-{arch}")).to_str().unwrap(),
                "--rootfs",
                root.join(format!("rootfs-{arch}.erofs")).to_str().unwrap(),
                "--arch",
                arch,
                "--output-dir",
                output.join(arch).to_str().unwrap(),
                "--binaries-json",
                if arch == "x86_64" {
                    binaries_json.to_str().unwrap()
                } else {
                    empty_binaries_json.to_str().unwrap()
                },
            ]);
        }

        run_boot_assets([
            "merge-manifest",
            output.join("arm64/manifest.json").to_str().unwrap(),
            output.join("x86_64/manifest.json").to_str().unwrap(),
            "--output",
            output.join("manifest.json").to_str().unwrap(),
        ]);

        publish_boot_assets(&output, &cdn_root);

        Self { output, cdn_root }
    }

    fn release_dir(&self, arch: &str) -> PathBuf {
        self.output.join(arch)
    }
}

fn write_runtime_binaries(cdn_root: &Path) -> Vec<Binary> {
    let direct_path = cdn_root.join("bin/direct-tool/1.0.0-e2e/x86_64/direct-tool");
    let runtime_path = cdn_root.join("bin/runtime-tool/2.0.0/x86_64/runtime-tool");
    write_executable(&direct_path, b"#!/bin/sh\necho direct-tool\n");
    write_executable(&runtime_path, b"#!/bin/sh\necho runtime-tool\n");

    vec![
        binary_entry(
            "direct-tool",
            "1.0.0+e2e",
            "bin/direct-tool/1.0.0-e2e/x86_64/direct-tool",
            &direct_path,
            None,
        ),
        binary_entry(
            "runtime-tool",
            "2.0.0",
            "bin/runtime-tool/2.0.0/x86_64/runtime-tool",
            &runtime_path,
            Some("runtime/bin"),
        ),
    ]
}

fn binary_entry(
    name: &str,
    version: &str,
    path: &str,
    file: &Path,
    install_dir: Option<&str>,
) -> Binary {
    Binary {
        name: name.to_string(),
        version: version.to_string(),
        targets: BTreeMap::from([(
            "x86_64".to_string(),
            BinaryTarget {
                path: path.to_string(),
                sha256: sha256_file(file),
            },
        )]),
        install_dir: install_dir.map(str::to_string),
    }
}

fn publish_boot_assets(output: &Path, cdn_root: &Path) {
    for arch in ["x86_64", "arm64"] {
        let target_dir = cdn_root.join(format!("asset/v{VERSION}/{arch}"));
        fs::create_dir_all(&target_dir).unwrap();
        extract_release_member(
            &output
                .join(arch)
                .join(format!("boot-assets-{arch}-v{VERSION}.tar.gz")),
            "kernel",
            &target_dir.join("kernel"),
        );
        extract_release_member(
            &output
                .join(arch)
                .join(format!("boot-assets-{arch}-v{VERSION}.tar.gz")),
            "rootfs.erofs",
            &target_dir.join("rootfs.erofs"),
        );
    }
    fs::copy(
        output.join("manifest.json"),
        cdn_root.join(format!("asset/v{VERSION}/manifest.json")),
    )
    .unwrap();
}

fn assert_release_tarball_contract(release_dir: &Path, arch: &str) {
    let tarball = release_dir.join(format!("boot-assets-{arch}-v{VERSION}.tar.gz"));
    assert_eq!(
        tar_members(&tarball),
        ["kernel", "manifest.json", "rootfs.erofs"]
    );
    assert_checksum_file_matches(&tarball);

    let manifest = read_manifest(&release_dir.join("manifest.json"));
    assert_eq!(manifest.asset_version, VERSION);
    assert_eq!(manifest.schema_version, 9);
    assert!(manifest.targets.contains_key(arch));
}

fn assert_manifest_contract(path: &Path) {
    let manifest = read_manifest(path);
    assert_eq!(manifest.asset_version, VERSION);
    assert_eq!(manifest.schema_version, 9);
    assert_eq!(
        manifest.targets.keys().collect::<Vec<_>>(),
        [&"arm64".to_string(), &"x86_64".to_string()]
    );

    let x86 = &manifest.targets["x86_64"];
    assert_eq!(x86.kernel.path, format!("asset/v{VERSION}/x86_64/kernel"));
    assert_eq!(
        x86.rootfs.path,
        format!("asset/v{VERSION}/x86_64/rootfs.erofs")
    );
    assert_eq!(
        x86.kernel.sha256,
        sha256_file(&path.parent().unwrap().join("x86_64/kernel"))
    );
    assert_eq!(
        x86.rootfs.sha256,
        sha256_file(&path.parent().unwrap().join("x86_64/rootfs.erofs"))
    );

    let direct = manifest
        .binaries
        .iter()
        .find(|binary| binary.name == "direct-tool")
        .unwrap();
    assert_eq!(direct.version, "1.0.0+e2e");
    assert_eq!(direct.install_dir, None);
    assert_eq!(
        direct.targets["x86_64"].path,
        "bin/direct-tool/1.0.0-e2e/x86_64/direct-tool"
    );

    let runtime = manifest
        .binaries
        .iter()
        .find(|binary| binary.name == "runtime-tool")
        .unwrap();
    assert_eq!(runtime.install_dir.as_deref(), Some("runtime/bin"));
}

fn boot_assets_bin() -> &'static str {
    env!("CARGO_BIN_EXE_boot-assets")
}

fn run_boot_assets<const N: usize>(args: [&str; N]) {
    let output = Command::new(boot_assets_bin()).args(args).output().unwrap();
    if !output.status.success() {
        panic!(
            "boot-assets failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn write_executable(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
}

fn assert_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(fs::metadata(path).unwrap().permissions().mode() & 0o111, 0);
    }
}

fn tar_members(path: &Path) -> Vec<String> {
    let file = fs::File::open(path).unwrap();
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let mut members = archive
        .entries()
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    members.sort();
    members
}

fn extract_release_member(tarball: &Path, member: &str, dest: &Path) {
    let file = fs::File::open(tarball).unwrap();
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.path().unwrap().to_string_lossy() == member {
            entry.unpack(dest).unwrap();
            return;
        }
    }
    panic!("{member} not found in {}", tarball.display());
}

fn assert_checksum_file_matches(tarball: &Path) {
    let checksum_path = tarball.with_file_name(format!(
        "{}.sha256",
        tarball.file_name().unwrap().to_string_lossy()
    ));
    let checksum = fs::read_to_string(checksum_path).unwrap();
    let expected = checksum.split_whitespace().next().unwrap();
    assert_eq!(sha256_file(tarball), expected);
}

fn read_manifest(path: &Path) -> Manifest {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn sha256_file(path: &Path) -> String {
    hex_encode(Sha256::digest(fs::read(path).unwrap()))
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

struct HttpServer {
    address: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl HttpServer {
    fn spawn(root: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve_http_file(stream, &root),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("test HTTP server failed: {error}"),
                }
            }
        });

        Self {
            address,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.address);
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
    }
}

fn serve_http_file(mut stream: TcpStream, root: &Path) {
    let mut request = [0; 4096];
    let bytes_read = stream.read(&mut request).unwrap_or(0);
    let request = String::from_utf8_lossy(&request[..bytes_read]);
    let Some(path) = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
    else {
        write_response(&mut stream, 400, b"bad request");
        return;
    };
    let relative = path.trim_start_matches('/');
    if relative.contains("..") {
        write_response(&mut stream, 403, b"forbidden");
        return;
    }
    let file = root.join(relative);
    match fs::read(file) {
        Ok(body) => write_response(&mut stream, 200, &body),
        Err(_) => write_response(&mut stream, 404, b"not found"),
    }
}

fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
}

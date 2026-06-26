use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use fs_err as fs;

use arcbox_boot::upstream::UpstreamConfig;
use arcbox_boot::upstream::UpstreamSource;
use arcbox_boot::upstream::UpstreamSourceFormat;
use arcbox_boot::util::{
    binary_object_path, cdn_url, path_safe_version, set_executable, sha256_file, write_json_pretty,
};

#[derive(Debug, Clone)]
pub struct SyncBinariesOpts {
    pub config: PathBuf,
    pub output: PathBuf,
    pub cdn_base_url: Option<String>,
    pub arches: Vec<String>,
    pub binaries_json: Option<PathBuf>,
}

pub fn sync_binaries(opts: &SyncBinariesOpts) -> Result<()> {
    let config = UpstreamConfig::from_file(&opts.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let arches: Vec<&str> = opts.arches.iter().map(String::as_str).collect();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        for binary in &config.binaries {
            for &arch in &arches {
                let source = binary
                    .source
                    .get(arch)
                    .with_context(|| format!("no source for {}/{arch}", binary.name))?;

                let safe_version = path_safe_version(&binary.version);
                let dest_dir = opts
                    .output
                    .join(&binary.name)
                    .join(&safe_version)
                    .join(arch);
                let dest_path = dest_dir.join(&binary.name);

                // Check if already downloaded locally.
                if dest_path.exists() {
                    println!(
                        "  [local]  {}/{} ({arch}) — {}",
                        binary.name,
                        binary.version,
                        dest_path.display()
                    );
                    continue;
                }

                // Check if already on CDN (skip download for upload purposes,
                // but still need a local copy for SHA256 manifest generation).
                if let Some(ref base) = opts.cdn_base_url {
                    let r2_path = binary_object_path(&binary.name, &safe_version, arch);
                    let url = cdn_url(base, &r2_path)?;
                    if check_cdn_exists(&url).await? {
                        println!(
                            "  [cdn]    {}/{} ({arch}) — downloading local copy for manifest",
                            binary.name, binary.version
                        );
                        // Still download locally so build_manifest_fragment can compute SHA256.
                        tokio::fs::create_dir_all(&dest_dir).await?;
                        download_source(source, &dest_path).await?;
                        continue;
                    }
                }

                println!(
                    "  [fetch]  {}/{} ({arch}) <- {}",
                    binary.name, binary.version, source.url
                );

                tokio::fs::create_dir_all(&dest_dir).await?;
                download_source(source, &dest_path).await?;

                let sha = sha256_file(&dest_path)?;
                println!("           sha256: {sha}");
            }
        }

        // Build and output manifest fragment.
        let fragment = build_manifest_fragment(&config, &opts.output, &arches)?;
        if let Some(ref out_path) = opts.binaries_json {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_json_pretty(out_path, &fragment)?;
            println!("==> Wrote binaries manifest to {}", out_path.display());
        }

        let json = serde_json::to_string_pretty(&fragment)?;
        println!();
        println!("=== manifest.json binaries fragment ===");
        println!("{json}");

        Ok(())
    })
}

async fn download_source(source: &UpstreamSource, dest: &std::path::Path) -> Result<()> {
    match source.format {
        UpstreamSourceFormat::Tgz => {
            let extract_path = source.extract.as_deref().ok_or_else(|| {
                anyhow::anyhow!("missing extract path for tgz source {}", source.url)
            })?;
            download_and_extract(&source.url, extract_path, dest).await
        }
        UpstreamSourceFormat::Binary => download_binary(&source.url, dest).await,
    }
}

async fn check_cdn_exists(url: &str) -> Result<bool> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = client.head(url).send().await;
    match resp {
        Ok(r) => Ok(r.status().is_success()),
        Err(_) => Ok(false),
    }
}

async fn download_and_extract(url: &str, extract_path: &str, dest: &std::path::Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        bail!("HTTP {} for {url}", resp.status());
    }

    let bytes = resp.bytes().await?;
    let gz = flate2::read::GzDecoder::new(bytes.as_ref());
    let mut archive = tar::Archive::new(gz);

    let mut found = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path.to_string_lossy() == extract_path {
            let mut content = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut content)?;
            fs::write(dest, &content)?;

            set_executable(dest)?;

            found = true;
            break;
        }
    }

    if !found {
        bail!("'{extract_path}' not found in archive from {url}");
    }
    Ok(())
}

async fn download_binary(url: &str, dest: &std::path::Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        bail!("HTTP {} for {url}", resp.status());
    }

    let bytes = resp.bytes().await?;
    fs::write(dest, &bytes)?;

    set_executable(dest)?;

    Ok(())
}

fn build_manifest_fragment(
    config: &UpstreamConfig,
    output: &std::path::Path,
    arches: &[&str],
) -> Result<Vec<arcbox_boot::manifest::Binary>> {
    use arcbox_boot::manifest::{Binary, BinaryTarget};
    use std::collections::BTreeMap;

    let mut result = Vec::new();
    for binary in &config.binaries {
        let mut targets = BTreeMap::new();
        let safe_version = path_safe_version(&binary.version);
        for &arch in arches {
            let dest = output
                .join(&binary.name)
                .join(&safe_version)
                .join(arch)
                .join(&binary.name);
            if dest.exists() {
                let sha = sha256_file(&dest)?;
                targets.insert(
                    arch.to_string(),
                    BinaryTarget {
                        path: binary_object_path(&binary.name, &safe_version, arch),
                        sha256: sha,
                    },
                );
            }
        }
        if !targets.is_empty() {
            result.push(Binary {
                name: binary.name.clone(),
                version: binary.version.clone(),
                targets,
                install_dir: binary.install_dir.clone(),
            });
        }
    }
    Ok(result)
}

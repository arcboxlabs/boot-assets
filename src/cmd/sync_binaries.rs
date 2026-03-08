use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use sha2::{Digest, Sha256};

use arcbox_boot::upstream::UpstreamConfig;
use arcbox_boot::upstream::UpstreamSource;
use arcbox_boot::upstream::UpstreamSourceFormat;

const ARCHES: &[&str] = &["arm64", "x86_64"];

#[derive(Args)]
pub struct SyncBinariesArgs {
    /// Path to upstream.toml.
    #[arg(long, default_value = "upstream.toml")]
    config: PathBuf,
    /// Output directory. Binaries are written to {output}/{name}/{version}/{arch}/{name}.
    #[arg(long, default_value = "dist/bin")]
    output: PathBuf,
    /// CDN base URL to check for existing binaries (skip download if present).
    #[arg(long)]
    cdn_base_url: Option<String>,
    /// Only process these architectures (comma-separated). Default: arm64,x86_64.
    #[arg(long, value_delimiter = ',')]
    arch: Option<Vec<String>>,
    /// Write binary manifest entries to this JSON file (for build-release --binaries-json).
    #[arg(long)]
    binaries_json: Option<PathBuf>,
}

impl SyncBinariesArgs {
    pub fn run(self) -> Result<()> {
        let config = UpstreamConfig::from_file(&self.config).map_err(|e| anyhow::anyhow!("{e}"))?;

        let arches: Vec<&str> = match &self.arch {
            Some(a) => a.iter().map(|s| s.as_str()).collect(),
            None => ARCHES.to_vec(),
        };

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

                    let dest_dir = self
                        .output
                        .join(&binary.name)
                        .join(&binary.version)
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
                    if let Some(ref base) = self.cdn_base_url {
                        let r2_path = format!(
                            "bin/{}/{}/{}/{}",
                            binary.name, binary.version, arch, binary.name
                        );
                        let url = format!("{}/{}", base.trim_end_matches('/'), r2_path);
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
            let fragment = build_manifest_fragment(&config, &self.output, &arches)?;
            let json = serde_json::to_string_pretty(&fragment)?;

            if let Some(ref out_path) = self.binaries_json {
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(out_path, &json)?;
                println!("==> Wrote binaries manifest to {}", out_path.display());
            }

            println!();
            println!("=== manifest.json binaries fragment ===");
            println!("{json}");

            Ok(())
        })
    }
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
            std::fs::write(dest, &content)?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(dest)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(dest, perms)?;
            }

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
    std::fs::write(dest, &bytes)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dest)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dest, perms)?;
    }

    Ok(())
}

fn sha256_file(path: &std::path::Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
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
        for &arch in arches {
            let dest = output
                .join(&binary.name)
                .join(&binary.version)
                .join(arch)
                .join(&binary.name);
            if dest.exists() {
                let sha = sha256_file(&dest)?;
                targets.insert(
                    arch.to_string(),
                    BinaryTarget {
                        path: format!(
                            "bin/{}/{}/{}/{}",
                            binary.name, binary.version, arch, binary.name
                        ),
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
            });
        }
    }
    Ok(result)
}

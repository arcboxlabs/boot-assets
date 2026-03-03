use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use sha2::{Digest, Sha256};

use arcbox_boot::upstream::UpstreamConfig;

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

                    // Check if already on CDN.
                    if let Some(ref base) = self.cdn_base_url {
                        let r2_path = format!(
                            "bin/{}/{}/{}/{}",
                            binary.name, binary.version, arch, binary.name
                        );
                        let url = format!("{}/{}", base.trim_end_matches('/'), r2_path);
                        if check_cdn_exists(&url).await? {
                            println!(
                                "  [cached] {}/{} ({arch}) — exists on CDN",
                                binary.name, binary.version
                            );
                            continue;
                        }
                    }

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

                    println!(
                        "  [fetch]  {}/{} ({arch}) <- {}",
                        binary.name, binary.version, source.url
                    );

                    tokio::fs::create_dir_all(&dest_dir).await?;
                    download_and_extract(&source.url, &source.extract, &dest_path).await?;

                    let sha = sha256_file(&dest_path)?;
                    println!("           sha256: {sha}");
                }
            }

            // Print manifest fragment for convenience.
            println!();
            println!("=== manifest.json binaries fragment ===");
            let fragment = build_manifest_fragment(&config, &self.output, &arches)?;
            println!("{}", serde_json::to_string_pretty(&fragment)?);

            Ok(())
        })
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

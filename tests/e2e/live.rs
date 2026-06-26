use std::env;

use crate::support::fetch_latest_version;
use arcbox_boot::asset_manager::{AssetManager, AssetManagerConfig};
use arcbox_boot::util::current_arch;

#[test]
#[ignore = "requires a published boot-assets CDN version"]
fn live_published_boot_assets_are_consumable() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let cdn_base_url = env::var("BOOT_ASSETS_LIVE_CDN_BASE_URL")
            .unwrap_or_else(|_| "https://boot.arcboxcdn.com".to_string());
        let version = match env::var("BOOT_ASSETS_LIVE_VERSION") {
            Ok(version) if !version.is_empty() => version,
            _ => fetch_latest_version(&cdn_base_url).await,
        };
        let arch = env::var("BOOT_ASSETS_LIVE_ARCH").unwrap_or_else(|_| current_arch().to_string());
        let prepare_binaries = env::var("BOOT_ASSETS_LIVE_PREPARE_BINARIES")
            .map(|value| value != "0" && value != "false")
            .unwrap_or(true);

        let temp = tempfile::tempdir().unwrap();
        let manager = AssetManager::new(AssetManagerConfig {
            cdn_base_url,
            version: version.clone(),
            arch: arch.clone(),
            cache_dir: temp.path().join("boot-cache"),
            custom_kernel: None,
        })
        .unwrap();

        let prepared = manager.prepare(None).await.unwrap();
        assert_eq!(prepared.version, version);
        assert!(prepared.kernel.is_file(), "kernel was not cached");
        assert!(prepared.rootfs.is_file(), "rootfs was not cached");
        assert!(
            prepared.manifest.targets.contains_key(&arch),
            "manifest is missing target arch {arch}"
        );
        assert!(
            !prepared.kernel_cmdline.is_empty(),
            "kernel command line must be present"
        );
        assert_ne!(
            std::fs::metadata(&prepared.kernel).unwrap().len(),
            0,
            "kernel is empty"
        );
        assert_ne!(
            std::fs::metadata(&prepared.rootfs).unwrap().len(),
            0,
            "rootfs is empty"
        );

        if prepare_binaries {
            let bin_dir = temp.path().join("guest/bin");
            manager.prepare_binaries(&bin_dir, None).await.unwrap();
            prepared
                .manifest
                .validate_binaries(&arch, &bin_dir)
                .await
                .unwrap();
        }
    });
}

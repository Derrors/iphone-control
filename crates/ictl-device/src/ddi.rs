//! 开发者镜像（iOS 17+ 的 Cryptex DDI）下载与缓存。
//!
//! 苹果的文件，不能随项目分发，运行时下载到数据目录。默认来源与 pymobiledevice3 相同：
//! 社区维护的 `doronz88/DeveloperDiskImage` 仓库；也可以指向自己从 Xcode 提取的目录（`ICTL_DDI_DIR`）。
//! 目录结构就是 `Cryptex1Assets::load` 要的 Restore 目录：BuildManifest.plist + 4 个载荷。

use std::path::{Path, PathBuf};

use crate::{DeviceError, Result};

pub const DEFAULT_BASE_URL: &str =
    "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/main/PersonalizedImages/Xcode_iOS_DDI_Cryptex";

/// 缓存目录里必须有的文件。载荷名由上游仓库重写后的 BuildManifest 声明，固定不变
pub const FILES: [&str; 5] =
    ["BuildManifest.plist", "Image.dmg", "Image.dmg.trustcache", "Image.dmg.cryptex_info", "Image.dmg.root_hash"];

/// 缓存是否完整
pub fn cache_complete(dir: &Path) -> bool {
    FILES.iter().all(|f| dir.join(f).is_file())
}

/// 确保 `dir` 里有完整的 DDI，缺的从 `base_url` 下载；返回该目录。
/// 下载先写临时文件再改名，中途失败不会留下半个文件。
pub async fn ensure_cache(dir: &Path, base_url: &str) -> Result<PathBuf> {
    if cache_complete(dir) {
        return Ok(dir.to_owned());
    }
    tokio::fs::create_dir_all(dir).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| DeviceError::Download(e.to_string()))?;
    for name in FILES {
        let dest = dir.join(name);
        if dest.is_file() {
            continue;
        }
        let url = format!("{}/{name}", base_url.trim_end_matches('/'));
        tracing::info!("下载开发者镜像 {url}");
        let resp = client
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| DeviceError::Download(format!("{name}: {e}")))?;
        let bytes = resp.bytes().await.map_err(|e| DeviceError::Download(format!("{name}: {e}")))?;
        let tmp = dir.join(format!(".{name}.part"));
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &dest).await?;
    }
    Ok(dir.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_complete_needs_all_five_files() {
        let dir = std::env::temp_dir().join(format!("ictl-ddi-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in &FILES[..4] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        assert!(!cache_complete(&dir));
        std::fs::write(dir.join(FILES[4]), b"x").unwrap();
        assert!(cache_complete(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

//! .ipa 解包和打包（就是 zip，里面是 `Payload/X.app`）。保留 Unix 权限位，否则可执行文件会丢掉 x 位。

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

/// 解包到 `dest`，返回 `Payload/` 下唯一的 .app 目录
pub fn unpack(ipa: &Path, dest: &Path) -> io::Result<PathBuf> {
    let mut zip = ZipArchive::new(File::open(ipa)?).map_err(io::Error::other)?;
    zip.extract(dest).map_err(io::Error::other)?;
    find_app(&dest.join("Payload"))
}

pub fn find_app(payload: &Path) -> io::Result<PathBuf> {
    std::fs::read_dir(payload)?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ipa 里没有 Payload/*.app"))
}

/// 把 `root` 下的 `Payload/` 打成 ipa
pub fn pack(root: &Path, out: &Path) -> io::Result<()> {
    let mut zip = ZipWriter::new(File::create(out)?);
    add_dir(&mut zip, root, &root.join("Payload"))?;
    zip.finish().map_err(io::Error::other)?;
    Ok(())
}

fn add_dir(zip: &mut ZipWriter<File>, root: &Path, dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        let rel = path.strip_prefix(root).map_err(io::Error::other)?.to_string_lossy().replace('\\', "/");
        let meta = std::fs::symlink_metadata(&path)?;
        let opts = SimpleFileOptions::default().unix_permissions(meta.permissions().mode() & 0o7777);
        if meta.is_dir() {
            zip.add_directory(format!("{rel}/"), opts).map_err(io::Error::other)?;
            add_dir(zip, root, &path)?;
        } else if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            zip.add_symlink(rel, target.to_string_lossy(), opts).map_err(io::Error::other)?;
        } else {
            zip.start_file(rel, opts).map_err(io::Error::other)?;
            let mut buf = Vec::new();
            File::open(&path)?.read_to_end(&mut buf)?;
            zip.write_all(&buf)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_then_unpack_keeps_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("ictl-ipa-{}", uuid::Uuid::new_v4()));
        let app = crate::bundle::tests::fake_wda(&root);
        let exe = app.join("WebDriverAgentRunner-Runner");
        std::fs::write(&exe, b"\xcf\xfa\xed\xfe").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ipa = root.join("out.ipa");
        pack(&root, &ipa).unwrap();

        let dest = root.join("unpacked");
        let app2 = unpack(&ipa, &dest).unwrap();
        assert_eq!(app2.file_name(), app.file_name());
        let mode = std::fs::metadata(app2.join("WebDriverAgentRunner-Runner")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        assert!(app2.join("PlugIns/WebDriverAgentRunner.xctest/Info.plist").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
}

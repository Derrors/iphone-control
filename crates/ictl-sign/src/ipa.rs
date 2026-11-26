//! .ipa 解包和打包（就是 zip，里面是 `Payload/X.app`）。保留 Unix 权限位，否则可执行文件会丢掉 x 位。

use std::fs::File;
use std::io;
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
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                ictl_l10n::text!("ipa 里没有 Payload/*.app", "no Payload/*.app in the ipa"),
            )
        })
}

/// 把 `root` 下的 `Payload/` 打成 ipa。写在旁边的临时文件里，写完才换上去：`out` 要么是原来的，要么是完整的新包
pub fn pack(root: &Path, out: &Path) -> io::Result<()> {
    write_atomically(out, |f| {
        let mut zip = ZipWriter::new(f);
        add_dir(&mut zip, root, &root.join("Payload"))?;
        zip.finish().map_err(io::Error::other)?;
        Ok(())
    })
}

/// 在 `out` 同一个目录里写一个临时文件，写完 fsync 后改名成 `out`；出错就删掉临时文件，`out` 不动
fn write_atomically(out: &Path, write: impl FnOnce(File) -> io::Result<()>) -> io::Result<()> {
    let name = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = out.with_file_name(format!(".{name}.{}.part", uuid::Uuid::new_v4()));
    let r = File::create(&tmp).and_then(|f| {
        let sync = f.try_clone()?;
        write(f)?;
        sync.sync_all()
    });
    let r = r.and_then(|()| std::fs::rename(&tmp, out));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// 根目录直接是 `X.app/` 的 zip（Appium 发布的 WDA 真机包）改成 ipa：每项加上 `Payload/` 前缀，
/// 文件原样复制压缩数据和权限位，不解压。目录项重新写：Appium 的包里目录项的类型位记成了普通文件
pub fn from_app_zip(src: &Path, out: &Path) -> io::Result<()> {
    let mut zip = ZipArchive::new(File::open(src)?).map_err(io::Error::other)?;
    // zip 9 起条目名不合法（例如带 `..`）时给的是错误：这种条目不算
    if !zip.file_names().flatten().any(|n| n.split('/').next().is_some_and(|top| top.ends_with(".app"))) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            ictl_l10n::text!("zip 根目录下没有 .app", "no .app at the root of the zip"),
        ));
    }
    write_atomically(out, |f| {
        let mut w = ZipWriter::new(f);
        w.add_directory("Payload/", SimpleFileOptions::default().unix_permissions(0o755)).map_err(io::Error::other)?;
        for i in 0..zip.len() {
            let f = zip.by_index_raw(i).map_err(io::Error::other)?;
            let name = format!("Payload/{}", f.name().map_err(io::Error::other)?);
            if f.is_dir() {
                let mode = f.unix_mode().map_or(0o755, |m| m & 0o7777);
                w.add_directory(name, SimpleFileOptions::default().unix_permissions(mode)).map_err(io::Error::other)?;
            } else {
                w.raw_copy_file_rename(f, name).map_err(io::Error::other)?;
            }
        }
        w.finish().map_err(io::Error::other)?;
        Ok(())
    })
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
            io::copy(&mut File::open(&path)?, zip)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A package that fails halfway leaves the old one as it was and nothing beside it
    #[test]
    fn a_failed_pack_leaves_the_old_package() {
        let root = std::env::temp_dir().join(format!("ictl-ipa-fail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let out = root.join("out.ipa");
        std::fs::write(&out, b"old").unwrap();
        assert!(pack(&root, &out).is_err(), "no Payload/ to pack");
        assert_eq!(std::fs::read(&out).unwrap(), b"old");
        let left: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("out.ipa")], "no temporary file left");
        assert!(from_app_zip(&root.join("missing.zip"), &out).is_err());
        assert_eq!(std::fs::read(&out).unwrap(), b"old");
        std::fs::remove_dir_all(root).unwrap();
    }

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

    #[test]
    fn app_zip_becomes_ipa() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("ictl-appzip-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        // 和 Appium 的 WebDriverAgentRunner-Runner.zip 一样：根目录就是 .app
        let src = root.join("wda.zip");
        let mut w = ZipWriter::new(File::create(&src).unwrap());
        let dir = SimpleFileOptions::default().unix_permissions(0o755);
        w.add_directory("WebDriverAgentRunner-Runner.app/", dir).unwrap();
        w.start_file("WebDriverAgentRunner-Runner.app/WebDriverAgentRunner-Runner", dir).unwrap();
        w.write_all(b"\xcf\xfa\xed\xfe").unwrap();
        w.start_file(
            "WebDriverAgentRunner-Runner.app/Info.plist",
            SimpleFileOptions::default().unix_permissions(0o644),
        )
        .unwrap();
        w.write_all(b"<plist/>").unwrap();
        w.finish().unwrap();

        let ipa = root.join("out.ipa");
        from_app_zip(&src, &ipa).unwrap();
        let app = unpack(&ipa, &root.join("unpacked")).unwrap();
        assert_eq!(app.file_name().unwrap(), "WebDriverAgentRunner-Runner.app");
        let mode = std::fs::metadata(app.join("WebDriverAgentRunner-Runner")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        assert_eq!(std::fs::read(app.join("Info.plist")).unwrap(), b"<plist/>");

        // 不是 .app 包的 zip 不收
        let bad = root.join("bad.zip");
        let mut w = ZipWriter::new(File::create(&bad).unwrap());
        w.start_file("README", SimpleFileOptions::default()).unwrap();
        w.finish().unwrap();
        assert!(from_app_zip(&bad, &root.join("bad.ipa")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

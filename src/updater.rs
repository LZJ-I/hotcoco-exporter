//! Public GitHub releases, fixed GH-Proxy route, and a pinned Windows CNG signing key.
use crate::{hashes::Hashes, net::Client};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const UPDATE_INDEX: &str = "hotcoco-exporter-update.json";
pub const UPDATE_ARCHIVE: &str = "hotcoco-exporter-windows-x64.zip";
pub const EXECUTABLE: &str = "HotCoco-资源导出工具.exe";
pub const PROXY: &str = "https://v4.gh-proxy.org/";
pub const MAX_ARCHIVE: usize = 32 * 1024 * 1024;
const REPOSITORY: &str = "LZJ-I/hotcoco-exporter";

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    size: u64,
    state: String,
    browser_download_url: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: String,
    pub version: String,
    pub tag: String,
    pub archive: FileInfo,
    pub executable: FileInfo,
    #[serde(default)]
    pub notes: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    payload: String,
    signature: String,
}
#[derive(Clone, Debug)]
pub struct Update {
    pub manifest: Manifest,
    pub envelope: Vec<u8>,
}

pub fn parse_version(value: &str) -> Result<[u32; 3]> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3 {
        bail!("无效工具版本");
    }
    let mut result = [0; 3];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty()
            || !part.bytes().all(|b| b.is_ascii_digit())
            || part.len() > 1 && part.starts_with('0')
        {
            bail!("无效工具版本");
        }
        result[index] = part.parse().context("无效工具版本")?;
    }
    Ok(result)
}
fn original_asset(tag: &str, name: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/{tag}/{name}")
}
fn checked_asset<'a>(release: &'a Release, name: &str, max: usize) -> Result<&'a Asset> {
    let assets: Vec<_> = release
        .assets
        .iter()
        .filter(|asset| asset.name == name)
        .collect();
    if assets.len() != 1 {
        bail!("新版 Release 的更新附件不完整，请稍后重试");
    }
    let asset = assets[0];
    if asset.state != "uploaded"
        || asset.size == 0
        || asset.size > max as u64
        || asset.browser_download_url != original_asset(&release.tag_name, name)
    {
        bail!("无效更新附件");
    }
    Ok(asset)
}

pub fn check(client: &Client, current: &str) -> Result<Option<Update>> {
    check_with(current, |url, max| client.update_bytes(url, max))
}
fn check_with(
    current: &str,
    mut get: impl FnMut(&str, usize) -> Result<Vec<u8>>,
) -> Result<Option<Update>> {
    let current = parse_version(current)?;
    let mut candidates = Vec::new();
    // Public unauthenticated requests. A bounded feed avoids depending on GitHub's "latest" ordering.
    for page in 1..=3 {
        let address = format!(
            "{PROXY}https://api.github.com/repos/{REPOSITORY}/releases?per_page=100&page={page}"
        );
        let releases: Vec<Release> = serde_json::from_slice(&get(&address, 2 * 1024 * 1024)?)
            .context("GitHub 版本列表无效")?;
        let count = releases.len();
        for release in releases {
            if release.draft || release.prerelease {
                continue;
            }
            let Some(version) = release
                .tag_name
                .strip_prefix('v')
                .and_then(|version| parse_version(version).ok())
            else {
                continue;
            };
            candidates.push((version, release));
        }
        if count < 100 {
            break;
        }
    }
    candidates.sort_by_key(|(version, _)| *version);
    let Some((version, release)) = candidates.pop() else {
        return Ok(None);
    };
    if version <= current {
        return Ok(None);
    }
    let index = checked_asset(&release, UPDATE_INDEX, 64 * 1024)?;
    let archive = checked_asset(&release, UPDATE_ARCHIVE, MAX_ARCHIVE)?;
    let envelope = get(&format!("{PROXY}{}", index.browser_download_url), 64 * 1024)?;
    let manifest = verify_envelope(&envelope)?;
    if parse_version(&manifest.version)? != version
        || manifest.tag != release.tag_name
        || manifest.archive.size != archive.size
    {
        bail!("更新清单与 Release 不一致");
    }
    Ok(Some(Update { manifest, envelope }))
}

pub fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if value.len() % 2 != 0 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("无效十六进制数据");
    }
    (0..value.len())
        .step_by(2)
        .map(|index| Ok(u8::from_str_radix(&value[index..index + 2], 16)?))
        .collect()
}
pub fn verify_envelope(bytes: &[u8]) -> Result<Manifest> {
    if bytes.len() > 64 * 1024 {
        bail!("更新清单过大");
    }
    let envelope: Envelope = serde_json::from_slice(bytes).context("更新清单无效")?;
    if envelope.payload.len() > 32 * 1024 {
        bail!("更新清单过大");
    }
    let signature = decode_hex(&envelope.signature)?;
    if signature.len() != 64 {
        bail!("更新签名无效");
    }
    verify_signature(envelope.payload.as_bytes(), &signature)?;
    let manifest: Manifest = serde_json::from_str(&envelope.payload)?;
    parse_version(&manifest.version)?;
    if manifest.schema_version != "hotcoco-exporter-update/1"
        || manifest.tag != format!("v{}", manifest.version)
        || manifest.archive.name != UPDATE_ARCHIVE
        || manifest.executable.name != EXECUTABLE
        || manifest.notes.len() > 8000
    {
        bail!("更新清单不匹配此工具");
    }
    for file in [&manifest.archive, &manifest.executable] {
        if file.size < 256
            || file.size > MAX_ARCHIVE as u64
            || file.sha256.len() != 64
            || !file.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            bail!("更新文件信息无效");
        }
    }
    Ok(manifest)
}
#[cfg(not(windows))]
fn verify_signature(_: &[u8], _: &[u8]) -> Result<()> {
    bail!("更新签名验证需要 Windows CNG")
}
#[cfg(windows)]
fn verify_signature(payload: &[u8], signature: &[u8]) -> Result<()> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Security::Cryptography::*;
    let blob = decode_hex(include_str!("update-public-key.hex").trim())?;
    let digest = decode_hex(&Hashes::sha256(payload)?)?;
    unsafe {
        let mut algorithm = null_mut();
        if BCryptOpenAlgorithmProvider(&mut algorithm, BCRYPT_ECDSA_P256_ALGORITHM, null(), 0) < 0 {
            bail!("无法初始化更新签名验证");
        }
        let mut key = null_mut();
        let imported = BCryptImportKeyPair(
            algorithm,
            null_mut(),
            BCRYPT_ECCPUBLIC_BLOB,
            &mut key,
            blob.as_ptr(),
            blob.len() as u32,
            0,
        );
        if imported < 0 {
            BCryptCloseAlgorithmProvider(algorithm, 0);
            bail!("更新公钥无效");
        }
        let status = BCryptVerifySignature(
            key,
            null(),
            digest.as_ptr(),
            digest.len() as u32,
            signature.as_ptr(),
            signature.len() as u32,
            0,
        );
        BCryptDestroyKey(key);
        BCryptCloseAlgorithmProvider(algorithm, 0);
        if status < 0 {
            bail!("更新签名校验失败，已停止更新");
        }
    }
    Ok(())
}
pub fn verify_archive(manifest: &Manifest, bytes: &[u8]) -> Result<Vec<u8>> {
    verify_file(&manifest.archive, bytes)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() != 1 {
        bail!("更新包仅应包含导出工具本体");
    }
    let mut file = archive.by_index(0)?;
    if file.name() != EXECUTABLE
        || file.is_dir()
        || file.size() != manifest.executable.size
        || file
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
    {
        bail!("更新包文件不匹配");
    }
    let mut executable = Vec::with_capacity(manifest.executable.size as usize);
    (&mut file)
        .take(manifest.executable.size + 1)
        .read_to_end(&mut executable)?;
    verify_file(&manifest.executable, &executable)?;
    validate_executable(&executable)?;
    Ok(executable)
}
pub fn verify_file(info: &FileInfo, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 != info.size || !Hashes::sha256(bytes)?.eq_ignore_ascii_case(&info.sha256)
    {
        bail!("更新文件大小或 SHA256 不匹配，原程序未改动");
    }
    Ok(())
}
pub fn validate_executable(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 256 || &bytes[..2] != b"MZ" {
        bail!("下载内容不是 Windows 程序");
    }
    let offset = u32::from_le_bytes(bytes[60..64].try_into()?) as usize;
    if offset.checked_add(26).is_none_or(|end| end > bytes.len())
        || &bytes[offset..offset + 4] != b"PE\0\0"
        || u16::from_le_bytes(bytes[offset + 4..offset + 6].try_into()?) != 0x8664
        || u16::from_le_bytes(bytes[offset + 24..offset + 26].try_into()?) != 0x20b
    {
        bail!("更新文件不是 Windows x64 程序");
    }
    Ok(())
}
pub fn download(client: &Client, update: &Update) -> Result<Vec<u8>> {
    let trusted = verify_envelope(&update.envelope)?;
    let bytes = client.update_bytes(
        &format!("{PROXY}{}", original_asset(&trusted.tag, UPDATE_ARCHIVE)),
        trusted.archive.size as usize,
    )?;
    verify_archive(&trusted, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[cfg(windows)]
    #[test]
    fn pinned_cng_signature_rejects_tampering_and_wrong_key() {
        let bytes = include_bytes!("test-data/signed-update.json");
        assert_eq!(verify_envelope(bytes).unwrap().version, "0.0.1");
        let mut envelope: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        envelope["payload"] = json!(envelope["payload"]
            .as_str()
            .unwrap()
            .replace("0.0.1", "9.0.0"));
        assert!(verify_envelope(&serde_json::to_vec(&envelope).unwrap()).is_err());
        let mut wrong_key: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        wrong_key["signature"] = json!("00".repeat(64));
        assert!(verify_envelope(&serde_json::to_vec(&wrong_key).unwrap()).is_err());
    }
    #[test]
    fn numeric_versions_and_stable_releases() {
        assert!(parse_version("0.10.0").unwrap() > parse_version("0.9.9").unwrap());
        for bad in [
            "1.2",
            "v1.2.3",
            "1.2.3-beta",
            "01.2.3",
            "1.2.-3",
            "4294967296.0.0",
        ] {
            assert!(parse_version(bad).is_err());
        }
        let releases = json!([
            {"tag_name":"v0.4.0-beta","draft":false,"prerelease":true,"assets":[]},
            {"tag_name":"v0.4.0","draft":true,"prerelease":false,"assets":[]},
            {"tag_name":"v0.2.0","draft":false,"prerelease":false,"assets":[]}
        ]);
        let mut calls = 0;
        assert!(check_with("0.3.0", |url, _| {
            calls += 1;
            assert!(url.starts_with(PROXY));
            Ok(serde_json::to_vec(&releases)?)
        })
        .unwrap()
        .is_none());
        assert_eq!(calls, 1);
    }
    #[test]
    fn incomplete_release_and_foreign_assets_rejected() {
        let releases = json!([{"tag_name":"v0.4.0","draft":false,"prerelease":false,"assets":[]}]);
        assert!(check_with("0.3.0", |_, _| Ok(serde_json::to_vec(&releases)?)).is_err());
        for url in [
            "http://v4.gh-proxy.org/https://api.github.com/repos/LZJ-I/hotcoco-exporter/releases",
            "https://v4.gh-proxy.org/https://evil.example/test",
            "https://github.com/other/repo/releases/download/v1.0.0/hotcoco-exporter-update.json",
            "https://github.com/LZJ-I/hotcoco-exporter/releases/download/v1.0.0/unknown.exe",
        ] {
            assert!(crate::net::allowed_update(url).is_err(), "{url}");
        }
        assert!(crate::net::allowed_update(&format!(
            "{PROXY}{}",
            original_asset("v0.4.0", UPDATE_INDEX)
        ))
        .is_ok());
    }
    #[test]
    fn wrong_architecture_and_html_rejected() {
        assert!(validate_executable(&vec![b'x'; 300]).is_err());
        let mut pe = vec![0; 300];
        pe[..2].copy_from_slice(b"MZ");
        pe[60..64].copy_from_slice(&128u32.to_le_bytes());
        pe[128..132].copy_from_slice(b"PE\0\0");
        pe[132..134].copy_from_slice(&0x8664u16.to_le_bytes());
        pe[152..154].copy_from_slice(&0x20bu16.to_le_bytes());
        assert!(validate_executable(&pe).is_ok());
        pe[132..134].copy_from_slice(&0x14cu16.to_le_bytes());
        assert!(validate_executable(&pe).is_err());
        pe[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(validate_executable(&pe).is_err());
    }
}

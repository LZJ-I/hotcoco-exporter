//! Resource identification, portable naming, reference packages and isolated download tests.
use crate::{
    hashes::{fingerprint, Hashes},
    net::{allowed_download, Client},
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub const INDEX: &str = "hotcoco.resources.json";
pub const LIMIT: usize = 256 * 1024 * 1024;
pub const SCHEMA: &str = "hotcoco-resource-export/0.2";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub provider: String,
    pub project_id: String,
    pub version_id: String,
    pub version: String,
    pub filename: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub resource_id: String,
    pub path: String,
    pub filename: String,
    pub kind: String,
    pub size: u64,
    pub hashes: Hashes,
    pub sources: Vec<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub resource: Resource,
    pub selected: bool,
    pub own: bool,
    pub status: String,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: String,
    pub scope: String,
    pub unsigned_test_artifact: bool,
    pub version: String,
    pub filename_policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<crate::instances::InstanceInfo>,
    pub files: Vec<Resource>,
}

#[derive(Clone)]
pub struct Cancel(pub Arc<AtomicBool>);
impl Default for Cancel {
    fn default() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
}
impl Cancel {
    pub fn check(&self) -> Result<()> {
        if self.0.load(Ordering::Relaxed) {
            bail!("操作已取消");
        }
        Ok(())
    }
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub fn validate_path(path: &str) -> Result<()> {
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() < 2 || !matches!(parts[0], "mods" | "resourcepacks" | "shaderpacks") {
        bail!("路径必须位于资源目录：{path}");
    }
    for part in parts {
        let base = part.split('.').next().unwrap_or("").to_uppercase();
        let reserved = matches!(
            base.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ["COM", "LPT"].iter().any(|prefix| {
            base.strip_prefix(prefix)
                .is_some_and(|s| s.chars().count() == 1 && "123456789¹²³".contains(s))
        });
        if part.is_empty()
            || matches!(part, "." | "..")
            || part.ends_with([' ', '.'])
            || reserved
            || part.chars().any(|c| c < ' ' || "<>:\"\\|?*".contains(c))
        {
            bail!("Windows 不支持此文件名：{path}");
        }
    }
    let suffix = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if !matches!(suffix.as_str(), "jar" | "zip") {
        bail!("仅支持 JAR / ZIP：{path}");
    }
    Ok(())
}

fn collect(folder: &Path, root: &Path, files: &mut Vec<(PathBuf, String)>) -> Result<()> {
    for item in fs::read_dir(folder)? {
        let item = item?;
        let file_type = item.file_type()?;
        // Never follow links into another instance or outside the selected resource tree.
        if file_type.is_symlink() {
            continue;
        }
        let path = item.path();
        if file_type.is_dir() {
            collect(&path, root, files)?;
        } else if file_type.is_file() {
            let suffix = path
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_lowercase();
            if matches!(suffix.as_str(), "jar" | "zip") {
                let relative = path
                    .strip_prefix(root)?
                    .to_str()
                    .context("文件名不是有效 Unicode")?
                    .replace('\\', "/");
                files.push((path, relative));
                if files.len() > 4096 {
                    bail!("单次最多扫描 4096 个资源");
                }
            }
        }
    }
    Ok(())
}

pub fn files_in(root: &Path) -> Result<Vec<(PathBuf, String)>> {
    if !root.is_dir() {
        bail!("请选择游戏实例目录");
    }
    let mut files = vec![];
    for dir in ["mods", "resourcepacks", "shaderpacks"] {
        let path = root.join(dir);
        if path.is_dir() && !fs::symlink_metadata(&path)?.file_type().is_symlink() {
            collect(&path, root, &mut files)?;
        }
    }
    files.sort_by(|a, b| a.1.cmp(&b.1));
    if files.is_empty() {
        bail!("此目录中没有资源。请选择包含 mods、resourcepacks 或 shaderpacks 的实例目录。");
    }
    Ok(files)
}

fn source(provider: &str, version: &Value, file: &Value, hashes: &Hashes) -> Result<Source> {
    let (project, id, label, name, url, check) = if provider == "modrinth" {
        (
            version["project_id"].clone(),
            version["id"].clone(),
            version["version_number"].clone(),
            file["filename"].clone(),
            file["url"].clone(),
            file["hashes"].clone(),
        )
    } else {
        let mut checks = serde_json::Map::new();
        for hash in file["hashes"].as_array().into_iter().flatten() {
            match hash["algo"].as_u64() {
                Some(1) => {
                    checks.insert("sha1".into(), hash["value"].clone());
                }
                Some(2) => {
                    checks.insert("md5".into(), hash["value"].clone());
                }
                _ => (),
            }
        }
        (
            file["modId"].clone(),
            file["id"].clone(),
            file["displayName"].clone(),
            file["fileName"].clone(),
            file["downloadUrl"].clone(),
            Value::Object(checks),
        )
    };
    if !hashes.agrees(&check) {
        bail!("来源返回的内容哈希不匹配");
    }
    let url = url
        .as_str()
        .context("已识别，但来源未提供下载地址")?
        .to_string();
    allowed_download(&url)?;
    let text_id = |v: Value| -> Result<String> {
        match v {
            Value::String(s) if !s.is_empty() => Ok(s),
            Value::Number(n) => Ok(n.to_string()),
            _ => bail!("来源缺少项目或版本 ID"),
        }
    };
    Ok(Source {
        provider: provider.into(),
        project_id: text_id(project)?,
        version_id: text_id(id)?,
        version: label.as_str().context("来源缺少版本号")?.into(),
        filename: name.as_str().context("来源缺少原文件名")?.into(),
        url,
    })
}

fn modrinth(client: &Client, hashes: &Hashes) -> Result<Option<Source>> {
    let mut error = None;
    for base in [
        "https://api.modrinth.com/v2",
        "https://mod.mcimirror.top/modrinth/v2",
    ] {
        match client.json(
            &format!("{base}/version_file/{}?algorithm=sha1", hashes.sha1),
            None,
        ) {
            Ok((404, _)) if base.contains("api.modrinth") => return Ok(None),
            Ok((404, _)) => error = Some(anyhow!("镜像尚未匹配，官方接口暂不可用")),
            Ok((_, version)) => {
                let file = version["files"]
                    .as_array()
                    .and_then(|files| {
                        files.iter().find(|f| {
                            f["hashes"]["sha1"]
                                .as_str()
                                .is_some_and(|h| h.eq_ignore_ascii_case(&hashes.sha1))
                        })
                    })
                    .context("版本响应中没有对应的文件")?;
                return Ok(Some(source("modrinth", &version, file, hashes)?));
            }
            Err(e) => error = Some(e),
        }
    }
    Err(error.unwrap_or_else(|| anyhow!("Modrinth 暂不可用")))
}

fn curseforge(client: &Client, data: &[u8], hashes: &Hashes) -> Result<Option<Source>> {
    let fingerprint = fingerprint(data);
    let (status, response) = client.json(
        "https://mod.mcimirror.top/curseforge/v1/fingerprints/432",
        Some(&json!({"fingerprints":[fingerprint]})),
    )?;
    if status == 404 {
        bail!("指纹接口暂不可用");
    }
    let matches = response["data"]["exactMatches"]
        .as_array()
        .context("指纹接口返回了无效数据")?;
    let mut error = None;
    for item in matches {
        let file = &item["file"];
        if file["fileFingerprint"].as_u64() == Some(fingerprint as u64) {
            match source("curseforge", &Value::Null, file, hashes) {
                Ok(s) => return Ok(Some(s)),
                Err(e) => error = Some(e),
            }
        }
    }
    match error {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

pub fn scan(
    root: &Path,
    client: &Client,
    cancel: &Cancel,
    mut progress: impl FnMut(usize, usize, Row),
) -> Result<Vec<Row>> {
    let root = root.canonicalize()?;
    let files = files_in(&root)?;
    let mut rows = vec![];
    let mut names = HashSet::new();
    for (file, path) in &files {
        cancel.check()?;
        let filename = file.file_name().unwrap().to_string_lossy().into_owned();
        let kind = match path.split('/').next().unwrap_or("") {
            "mods" => "mod",
            "resourcepacks" => "resourcepack",
            _ => "shader",
        };
        let mut row = Row {
            resource: Resource {
                resource_id: String::new(),
                path: path.clone(),
                filename,
                kind: kind.into(),
                size: 0,
                hashes: Hashes::default(),
                sources: vec![],
                embedded: None,
            },
            selected: true,
            own: false,
            status: "待处理".into(),
            note: String::new(),
        };
        let result: Result<()> = (|| {
            validate_path(path)?;
            if !names.insert(path.to_lowercase()) {
                bail!("文件名大小写冲突");
            }
            if !file.canonicalize()?.starts_with(&root) {
                bail!("文件位于实例目录之外");
            }
            let before = file.metadata()?;
            if before.len() == 0 || before.len() > LIMIT as u64 {
                bail!("文件须为 1 B 至 256 MiB");
            }
            let data = fs::read(file)?;
            let hashes = Hashes::of(&data)?;
            let after = file.metadata()?;
            if before.len() != after.len() || before.modified()? != after.modified()? {
                bail!("扫描期间文件发生变化，请重新识别");
            }
            row.resource.size = data.len() as u64;
            row.resource.hashes = hashes;
            let mut notes = vec![];
            cancel.check()?;
            match modrinth(client, &row.resource.hashes) {
                Ok(Some(s)) => row.resource.sources.push(s),
                Ok(None) => notes.push("Modrinth 未收录".to_string()),
                Err(e) => notes.push(format!("Modrinth：{e}")),
            }
            cancel.check()?;
            match curseforge(client, &data, &row.resource.hashes) {
                Ok(Some(s)) => row.resource.sources.push(s),
                Ok(None) => notes.push("CurseForge 未收录".to_string()),
                Err(e) => notes.push(format!("CurseForge：{e}")),
            }
            if let Some(s) = row.resource.sources.first() {
                row.resource.resource_id = format!("{}:{}", s.provider, s.project_id);
                row.status = "已识别".into();
                if s.filename != row.resource.filename {
                    notes.insert(0, format!("保留自定义名；上游原名：{}", s.filename));
                }
            } else {
                notes.insert(0, "可排除此文件，或明确标记为自有资源".into());
            }
            row.note = notes.join(" · ");
            Ok(())
        })();
        if let Err(e) = result {
            row.status = "失败".into();
            row.note = e.to_string();
        }
        progress(rows.len() + 1, files.len(), row.clone());
        rows.push(row);
    }
    cancel.check()?;
    Ok(rows)
}

pub fn manifest(rows: &[Row], version: &str) -> Result<Manifest> {
    let version = version.trim();
    if version.is_empty() || version.len() > 120 {
        bail!("请填写 1 至 120 字节的发布版本号");
    }
    let mut files = vec![];
    for row in rows.iter().filter(|r| r.selected) {
        let mut file = row.resource.clone();
        if row.own && file.size > 0 {
            file.resource_id = format!("custom:{}", file.path);
            file.embedded = Some(format!("files/{}", file.path));
            file.sources.clear();
        } else if file.sources.is_empty() {
            bail!("请处理或排除文件：{}", file.path);
        }
        files.push(file);
    }
    let result = Manifest {
        schema_version: SCHEMA.into(),
        scope: "resources-only".into(),
        unsigned_test_artifact: true,
        version: version.into(),
        filename_policy: "publisher path is deployment path; upstream name is metadata only".into(),
        instance: None,
        files,
    };
    validate_manifest(&result)?;
    Ok(result)
}

pub fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.schema_version != SCHEMA
        || manifest.scope != "resources-only"
        || manifest.files.is_empty()
        || manifest.files.len() > 4096
    {
        bail!("不支持或为空的资源清单");
    }
    let mut paths = HashSet::new();
    let mut identities = HashSet::new();
    for file in &manifest.files {
        validate_path(&file.path)?;
        if file.filename != file.path.rsplit('/').next().unwrap_or("") {
            bail!("安装名与路径不一致");
        }
        if !paths.insert(file.path.to_lowercase()) || !identities.insert(file.resource_id.clone()) {
            bail!("重复的安装路径或资源项目：{}", file.path);
        }
        if file.size == 0 || file.size > LIMIT as u64 {
            bail!("无效资源大小");
        }
        for (hash, length) in [
            (&file.hashes.sha1, 40),
            (&file.hashes.sha256, 64),
            (&file.hashes.sha512, 128),
            (&file.hashes.md5, 32),
        ] {
            if hash.len() != length || !hash.bytes().all(|x| x.is_ascii_hexdigit()) {
                bail!("无效内容哈希");
            }
        }
        if let Some(embedded) = &file.embedded {
            if embedded != &format!("files/{}", file.path) || !file.sources.is_empty() {
                bail!("无效的自有资源路径");
            }
        } else {
            if file.sources.is_empty() || file.sources.len() > 8 {
                bail!("资源没有下载来源");
            }
            for source in &file.sources {
                if !matches!(source.provider.as_str(), "modrinth" | "curseforge")
                    || source.project_id.is_empty()
                    || source.version_id.is_empty()
                {
                    bail!("无效下载来源");
                }
                allowed_download(&source.url)?;
            }
        }
    }
    for path in &paths {
        for parent in Path::new(path).ancestors().skip(1) {
            if paths.contains(parent.to_string_lossy().as_ref()) {
                bail!("文件与目录路径冲突");
            }
        }
    }
    Ok(())
}

fn verify(data: &[u8], resource: &Resource) -> Result<()> {
    if data.len() as u64 != resource.size || Hashes::of(data)? != resource.hashes {
        bail!("文件大小或哈希不匹配：{}", resource.path);
    }
    Ok(())
}

pub fn export(root: &Path, manifest: &Manifest, output: &Path, cancel: &Cancel) -> Result<()> {
    validate_manifest(manifest)?;
    let root = root.canonicalize()?;
    let parent = output.parent().context("无效导出位置")?;
    fs::create_dir_all(parent)?;
    if parent.canonicalize()?.starts_with(&root) {
        bail!("请选择实例目录以外的导出位置");
    }
    let temporary = output.with_extension(format!("partial-{}", std::process::id()));
    if output.exists() || temporary.exists() {
        bail!("文件已存在，请选择新的导出文件名");
    }
    let result: Result<()> = (|| {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(INDEX, options)?;
        zip.write_all(&serde_json::to_vec_pretty(manifest)?)?;
        for resource in &manifest.files {
            cancel.check()?;
            let path = root.join(&resource.path);
            if !path.canonicalize()?.starts_with(&root)
                || fs::symlink_metadata(&path)?.file_type().is_symlink()
            {
                bail!("文件路径已改变，请重新扫描");
            }
            let data = fs::read(path)?;
            verify(&data, resource).context("本地文件已改变，请重新扫描")?;
            if let Some(embedded) = &resource.embedded {
                zip.start_file(embedded, options)?;
                zip.write_all(&data)?;
            }
        }
        zip.finish()?.sync_all()?;
        cancel.check()?;
        if output.exists() {
            bail!("导出文件已存在");
        }
        fs::rename(&temporary, output)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn restore(
    package: &Path,
    destination: &Path,
    client: &Client,
    preferred: Option<&str>,
    cancel: &Cancel,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<Value> {
    let mut archive = zip::ZipArchive::new(File::open(package)?)?;
    let manifest: Manifest = {
        let entry = archive.by_name(INDEX)?;
        if entry.size() > 8 * 1024 * 1024 {
            bail!("清单过大");
        }
        serde_json::from_reader(entry.take(8 * 1024 * 1024))?
    };
    validate_manifest(&manifest)?;
    let expected: HashSet<_> = std::iter::once(INDEX.to_string())
        .chain(manifest.files.iter().filter_map(|f| f.embedded.clone()))
        .collect();
    if archive.len() != expected.len() || archive.file_names().any(|name| !expected.contains(name))
    {
        bail!("导出包包含未声明或重复的文件");
    }
    if destination.exists() {
        bail!("下载验证仅允许使用新目录，已有实例不会被覆盖");
    }
    fs::create_dir_all(destination.parent().context("无效验证目录")?)?;
    fs::create_dir(destination)?;
    let staging = destination.join(".hotcoco-staging");
    fs::create_dir(&staging)?;
    let mut receipts = vec![];
    for (index, resource) in manifest.files.iter().enumerate() {
        cancel.check()?;
        let (data, chosen, failures) = if let Some(embedded) = &resource.embedded {
            let mut data = vec![];
            archive
                .by_name(embedded)?
                .take(resource.size + 1)
                .read_to_end(&mut data)?;
            verify(&data, resource)?;
            (
                data,
                json!({"provider":"embedded", "filename":resource.filename}),
                vec![],
            )
        } else {
            let mut sources: Vec<_> = resource.sources.iter().collect();
            sources.sort_by_key(|s| Some(s.provider.as_str()) != preferred);
            let mut chosen = None;
            let mut failures = vec![];
            for source in sources {
                cancel.check()?;
                match client
                    .download(&source.url, resource.size as usize)
                    .and_then(|data| {
                        verify(&data, resource)?;
                        Ok(data)
                    }) {
                    Ok(data) => {
                        chosen = Some((data, serde_json::to_value(source)?));
                        break;
                    }
                    Err(e) => failures.push(format!("{}：{e}", source.provider)),
                }
            }
            let (data, chosen) = chosen
                .ok_or_else(|| anyhow!("下载失败：{}\n{}", resource.path, failures.join("\n")))?;
            (data, chosen, failures)
        };
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(staging.join(index.to_string()))?;
        file.write_all(&data)?;
        file.sync_all()?;
        receipts.push(json!({"path":resource.path, "filename":resource.filename, "hashes":resource.hashes, "source":chosen, "failedAttempts":failures}));
        progress(index + 1, manifest.files.len(), &resource.path);
    }
    cancel.check()?;
    let canonical = destination.canonicalize()?;
    for (index, resource) in manifest.files.iter().enumerate() {
        let target = destination.join(&resource.path);
        let parent = target.parent().unwrap();
        fs::create_dir_all(parent)?;
        if !parent.canonicalize()?.starts_with(&canonical) || target.exists() {
            bail!("验证目录已发生变化");
        }
        fs::rename(staging.join(index.to_string()), target)?;
    }
    fs::remove_dir(staging)?;
    let receipt = json!({"testOnly":true, "version":manifest.version, "files":receipts});
    write_json(&destination.join("installed-ledger.json"), &receipt)?;
    Ok(receipt)
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_preserve_aliases() {
        assert!(validate_path("mods/[优化] 钠 Sodium.jar").is_ok());
    }
    #[test]
    fn traversal_and_reserved_names_rejected() {
        for path in [
            "../mods/a.jar",
            "mods/../../a.jar",
            "mods/NUL.jar",
            "mods/a.jar:stream",
            "C:/a.jar",
            "mods/a\\b.jar",
            "mods/a.jar.",
        ] {
            assert!(validate_path(path).is_err(), "{path}");
        }
    }
    #[test]
    fn source_url_allowlist() {
        for url in [
            "http://cdn.modrinth.com/a",
            "https://127.0.0.1/a",
            "https://cdn.modrinth.com.evil.test/a",
            "file:///a",
        ] {
            assert!(allowed_download(url).is_err());
        }
    }
    #[test]
    fn fingerprint_whitespace() {
        assert_eq!(fingerprint(b"ab c\td\ne\r"), fingerprint(b"abcde"));
        assert_ne!(fingerprint(b"abcde"), fingerprint(b"abcdef"));
    }
    #[test]
    fn hash_mismatch_cannot_be_source() {
        let hashes = Hashes {
            sha1: "correct".into(),
            ..Hashes::default()
        };
        assert!(!hashes.agrees(&json!({"sha1":"wrong"})));
        assert!(!hashes.agrees(&json!({})));
    }
}

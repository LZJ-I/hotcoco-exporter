//! Discover adjacent Minecraft repositories and select exactly one isolated instance.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoaderInfo {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceInfo {
    pub id: String,
    pub layout: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minecraft_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<LoaderInfo>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub label: String,
    pub root: PathBuf,
    pub info: InstanceInfo,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovery {
    pub location: PathBuf,
    pub instances: Vec<Instance>,
}

fn resource_dirs(path: &Path) -> bool {
    ["mods", "resourcepacks", "shaderpacks"]
        .iter()
        .any(|name| path.join(name).is_dir())
}

fn directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
}

fn version_json(root: &Path) -> Option<Value> {
    let id = root.file_name()?.to_str()?;
    let mut files = vec![root.join(format!("{id}.json"))];
    if let Ok(entries) = fs::read_dir(root) {
        let mut alternatives: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|x| x.to_str()) == Some("json") && !files.contains(p)
            })
            .collect();
        alternatives.sort();
        files.extend(alternatives.into_iter().take(32));
    }
    for file in files {
        if fs::metadata(&file)
            .ok()
            .is_none_or(|m| m.len() > 2 * 1024 * 1024)
        {
            continue;
        }
        let mut bytes = vec![];
        let Ok(file) = File::open(file) else {
            continue;
        };
        if file
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
            continue;
        };
        if value["id"].is_string()
            && (value["mainClass"].is_string()
                || value["inheritsFrom"].is_string()
                || value["libraries"].is_array()
                || value["downloads"].is_object())
        {
            return Some(value);
        }
    }
    None
}

fn instance(root: &Path, isolated: bool, prefix: Option<&str>) -> Instance {
    let id = root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let metadata = version_json(root);
    let minecraft_version = metadata
        .as_ref()
        .and_then(|v| {
            v["inheritsFrom"]
                .as_str()
                .or_else(|| v["clientVersion"].as_str())
                .or_else(|| v["id"].as_str())
        })
        .map(String::from);
    let loader = metadata
        .as_ref()
        .and_then(|v| v["libraries"].as_array())
        .and_then(|libs| {
            libs.iter()
                .filter_map(|lib| lib["name"].as_str())
                .find_map(|name| {
                    for (prefix, label) in [
                        ("net.fabricmc:fabric-loader:", "Fabric"),
                        ("net.minecraftforge:forge:", "Forge"),
                        ("net.neoforged:neoforge:", "NeoForge"),
                        ("org.quiltmc:quilt-loader:", "Quilt"),
                    ] {
                        if let Some(version) = name.strip_prefix(prefix) {
                            return Some(LoaderInfo {
                                name: label.into(),
                                version: version.into(),
                            });
                        }
                    }
                    None
                })
        });
    let label = match prefix {
        Some(prefix) => format!("{prefix} / {id}"),
        None => id.clone(),
    };
    Instance {
        label,
        root: root.to_path_buf(),
        info: InstanceInfo {
            id,
            layout: if isolated {
                "version-isolated"
            } else {
                "standalone"
            }
            .into(),
            minecraft_version,
            loader,
        },
    }
}

fn repository(root: &Path, prefix: Option<&str>, result: &mut Vec<Instance>) -> Result<()> {
    let start = result.len();
    let versions = root.join("versions");
    if directory(&versions) {
        for entry in fs::read_dir(&versions)? {
            let path = entry?.path();
            if directory(&path) && (resource_dirs(&path) || version_json(&path).is_some()) {
                result.push(instance(&path, true, prefix));
                if result.len() > 256 {
                    bail!("检测到的包超过 256 个，请直接选择目标版本文件夹");
                }
            }
        }
    }
    // A repository's shared resources are not an automatic fallback for an isolated version.
    if result.len() == start && resource_dirs(root) {
        result.push(instance(root, false, prefix));
    }
    Ok(())
}

pub fn discover(location: &Path) -> Result<Discovery> {
    let location = location.canonicalize().context("无法读取所选文件夹")?;
    if !location.is_dir() {
        bail!("请选择文件夹");
    }
    let mut result = vec![];
    let is_version = location
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("versions"));
    if is_version && (resource_dirs(&location) || version_json(&location).is_some()) {
        result.push(instance(&location, true, None));
    } else if directory(&location.join(".minecraft")) {
        repository(&location.join(".minecraft"), None, &mut result)?;
    } else if directory(&location.join("versions")) || resource_dirs(&location) {
        repository(&location, None, &mut result)?;
    } else {
        // Explicitly selected collections may contain several pack folders, each with .minecraft.
        let mut children: Vec<_> = fs::read_dir(&location)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|path| directory(path) && directory(&path.join(".minecraft")))
            .collect();
        children.sort();
        if children.len() > 64 {
            bail!("候选目录过多，请选择具体游戏目录");
        }
        for child in children {
            let prefix = child.file_name().unwrap_or_default().to_string_lossy();
            repository(&child.join(".minecraft"), Some(&prefix), &mut result)?;
        }
    }
    result.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    Ok(Discovery {
        location,
        instances: result,
    })
}

pub fn automatic_from(executable_dir: &Path, current_dir: &Path) -> Result<Option<Discovery>> {
    // Search bounded anchors only. The EXE location takes precedence over its working directory.
    for anchor in [executable_dir, current_dir] {
        if !anchor.is_dir() {
            continue;
        }
        let discovery = discover(anchor)?;
        if !discovery.instances.is_empty() {
            return Ok(Some(discovery));
        }
    }
    Ok(None)
}

pub fn automatic() -> Result<Option<Discovery>> {
    let executable = std::env::current_exe()?;
    automatic_from(
        executable.parent().context("无法读取工具位置")?,
        &std::env::current_dir()?,
    )
}

pub fn select(location: &Path, requested: Option<&str>) -> Result<Instance> {
    let discovery = discover(location)?;
    if let Some(requested) = requested {
        let mut found = discovery
            .instances
            .into_iter()
            .filter(|i| i.info.id == requested || i.label == requested);
        let first = found.next().context("未找到指定的包／版本")?;
        if found.next().is_some() {
            bail!("包名重复，请使用完整显示名称或直接选择版本文件夹");
        }
        return Ok(first);
    }
    match discovery.instances.as_slice() {
        [only] => Ok(only.clone()),
        [] => bail!("未检测到游戏包，请选择 .minecraft、其同级目录，或具体版本文件夹"),
        many => bail!(
            "检测到多个包，请选择一个版本（命令行使用 --instance）：{}",
            many.iter()
                .map(|i| i.label.as_str())
                .collect::<Vec<_>>()
                .join("、")
        ),
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
pub fn display(path: &Path) -> String {
    let value = path.to_string_lossy();
    if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(&value).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/discovery-tests")
                .join(format!(
                    "{}-{}-{}",
                    std::process::id(),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    SEQUENCE.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn version(&self, name: &str) -> PathBuf {
            let root = self.0.join(".minecraft/versions").join(name);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join(format!("{name}.json")),r#"{"id":"custom","inheritsFrom":"1.21.1","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.16.10"}]}"#).unwrap();
            root
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let base = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/discovery-tests")
                .canonicalize()
                .unwrap();
            if self.0.canonicalize().unwrap().starts_with(base) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn sibling_minecraft_lists_isolated_versions_without_shared_resources() {
        let f = Fixture::new();
        let a = f.version("HotCoco-A");
        let b = f.version("HotCoco-B");
        for root in [&a, &b, &f.0.join(".minecraft")] {
            fs::create_dir_all(root.join("mods")).unwrap();
            fs::write(root.join("mods/example.jar"), b"sample").unwrap();
        }
        let discovered = discover(&f.0).unwrap();
        assert_eq!(discovered.instances.len(), 2);
        let selected = select(&f.0, Some("HotCoco-A")).unwrap();
        assert_eq!(selected.root, a.canonicalize().unwrap());
        let files = crate::core::files_in(&selected.root).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].0.starts_with(a.canonicalize().unwrap()));
        assert_eq!(selected.info.minecraft_version.as_deref(), Some("1.21.1"));
        assert_eq!(selected.info.loader.unwrap().name, "Fabric");
    }
    #[test]
    fn multiple_versions_require_explicit_selection() {
        let f = Fixture::new();
        f.version("A");
        f.version("B");
        assert!(select(&f.0, None).is_err());
    }
    #[test]
    fn directly_selected_version_never_expands_to_siblings() {
        let f = Fixture::new();
        let a = f.version("版本 A");
        f.version("版本 B");
        let d = discover(&a).unwrap();
        assert_eq!(d.instances.len(), 1);
        assert_eq!(d.instances[0].info.id, "版本 A");
    }
    #[test]
    fn empty_version_never_falls_back_to_shared_mods() {
        let f = Fixture::new();
        let empty = f.version("空版本");
        let shared = f.0.join(".minecraft/mods");
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join("shared.jar"), b"shared").unwrap();
        let selected = select(&f.0, None).unwrap();
        assert_eq!(selected.root, empty.canonicalize().unwrap());
        assert!(crate::core::files_in(&selected.root).is_err());
    }
    #[test]
    fn manually_selected_standalone_resources_remain_supported() {
        let f = Fixture::new();
        fs::create_dir_all(f.0.join("mods")).unwrap();
        let i = select(&f.0, None).unwrap();
        assert_eq!(i.info.layout, "standalone");
    }
    #[test]
    fn executable_directory_precedes_unrelated_working_directory() {
        let near = Fixture::new();
        near.version("附近的包");
        let cwd = Fixture::new();
        cwd.version("无关的包");
        let d = automatic_from(&near.0, &cwd.0).unwrap().unwrap();
        assert_eq!(d.instances[0].info.id, "附近的包");
    }
    #[test]
    fn renamed_version_json_is_detected_without_rewriting_it() {
        let f = Fixture::new();
        let root = f.version("本地改名");
        let old = root.join("本地改名.json");
        let renamed = root.join("upstream.json");
        fs::rename(old, &renamed).unwrap();
        let d = discover(&f.0).unwrap();
        assert_eq!(d.instances.len(), 1);
        assert_eq!(d.instances[0].info.id, "本地改名");
        assert!(renamed.exists());
    }
    #[test]
    fn windows_utf8_bom_manifest_is_supported() {
        let f = Fixture::new();
        let root = f.version("BOM版本");
        let path = root.join("BOM版本.json");
        let mut bytes = vec![0xef, 0xbb, 0xbf];
        bytes.extend_from_slice(&fs::read(&path).unwrap());
        fs::write(&path, bytes).unwrap();
        let i = select(&f.0, None).unwrap();
        assert_eq!(i.info.minecraft_version.as_deref(), Some("1.21.1"));
    }
    #[test]
    fn unrelated_settings_folder_is_not_a_version() {
        let f = Fixture::new();
        f.version("有效版本");
        let other = f.0.join(".minecraft/versions/缓存");
        fs::create_dir_all(&other).unwrap();
        fs::write(
            other.join("settings.json"),
            r#"{"id":"cache","theme":"dark"}"#,
        )
        .unwrap();
        let d = discover(&f.0).unwrap();
        assert_eq!(d.instances.len(), 1);
        assert_eq!(d.instances[0].info.id, "有效版本");
    }
    #[test]
    fn multiple_pack_collection_is_listed_separately() {
        let f = Fixture::new();
        for pack in ["客户端甲", "客户端乙"] {
            let root = f.0.join(pack).join(".minecraft/versions/同名版本/mods");
            fs::create_dir_all(root).unwrap();
        }
        let d = discover(&f.0).unwrap();
        assert_eq!(d.instances.len(), 2);
        assert!(select(&f.0, Some("同名版本")).is_err());
        assert!(select(&f.0, Some("客户端甲 / 同名版本")).is_ok());
    }
}

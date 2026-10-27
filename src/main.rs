#![cfg_attr(windows, windows_subsystem = "windows")]
mod core;
#[cfg(windows)]
mod gui;
mod hashes;
mod instances;
mod net;
#[cfg(windows)]
mod update_install;
mod updater;

use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
};

fn value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|p| p[0] == flag).map(|p| p[1].clone())
}
fn required(args: &[String], flag: &str) -> Result<String> {
    value(args, flag).with_context(|| format!("缺少参数 {flag}"))
}

fn command(args: &[String]) -> Result<()> {
    let client = net::Client {
        proxy: value(args, "--proxy"),
    };
    let cancel = core::Cancel::default();
    match args.first().map(String::as_str) {
        None|Some("--proxy")|Some("--update-result")=>{
            #[cfg(windows)] return gui::run(client.proxy,value(args,"--update-result").as_deref().map(Path::new));
            #[cfg(not(windows))] bail!("图形界面需要 Windows");
        },
        Some("--preview")=>{
            #[cfg(windows)] return gui::preview(Path::new(args.get(1).context("缺少 PNG 输出位置")?),args.iter().any(|a|a=="--empty"));
            #[cfg(not(windows))] bail!("离屏预览需要 Windows");
        },
        Some("scan")|Some("export")=>{
            let selected=instances::select(Path::new(args.get(1).context("缺少实例目录")?),value(args,"--instance").as_deref())?;
            let root=selected.root;
            let mut rows=core::scan(&root,&client,&cancel,|done,total,row|println!("{done}/{total} {} {}",row.status,row.resource.path))?;
            for pair in args.windows(2) {
                if matches!(pair[0].as_str(),"--own"|"--exclude") {
                    let row=rows.iter_mut().find(|r|r.resource.path==pair[1]).with_context(||format!("清单中没有文件：{}",pair[1]))?;
                    if pair[0]=="--own"{row.own=true;}else{row.selected=false;}
                }
            }
            if args[0]=="export" {
                let mut manifest=core::manifest(&rows,&required(args,"--version")?)?;
                manifest.instance=Some(selected.info);
                core::export(&root,&manifest,Path::new(&required(args,"--output")?),&cancel)?;
            }
            if let Some(report)=value(args,"--report"){core::write_json(Path::new(&report),&rows)?;}
        },
        Some("discover")=>{
            let discovery=if args.get(1).is_some_and(|a|!a.starts_with("--")) {Some(instances::discover(Path::new(&args[1]))?)} else {instances::automatic()?};
            if let Some(path)=value(args,"--report"){core::write_json(Path::new(&path),&discovery)?;}
            println!("{}",serde_json::to_string(&discovery)?);
        },
        Some("restore")=>{
            let package=PathBuf::from(args.get(1).context("缺少导出包路径")?);
            let report=core::restore(&package,Path::new(&required(args,"--destination")?),&client,value(args,"--provider").as_deref(),&cancel,
                |done,total,path|println!("{done}/{total} VERIFIED {path}"))?;
            if let Some(path)=value(args,"--report"){core::write_json(Path::new(&path),&report)?;}
        },
        Some("self-test")=>{
            let report=self_test(Path::new(&required(args,"--workdir")?))?;
            if let Some(path)=value(args,"--report"){core::write_json(Path::new(&path),&report)?;}
            println!("{}",serde_json::to_string(&report)?);
        },
        Some("check-update")|Some("download-update")=>{
            let current=value(args,"--current-version").unwrap_or_else(||updater::VERSION.into());
            let update=updater::check(&client,&current)?;
            let mut report=json!({"currentVersion":current,"available":update.is_some(),"proxy":updater::PROXY,"repository":"LZJ-I/hotcoco-exporter"});
            if let Some(update)=update {
                report["version"]=json!(update.manifest.version);
                if args[0]=="download-update"{
                    use std::io::Write;
                    let bytes=updater::download(&client,&update)?;
                    let destination=PathBuf::from(required(args,"--output")?);
                    let mut file=fs::OpenOptions::new().create_new(true).write(true).open(&destination)?;
                    file.write_all(&bytes)?;file.sync_all()?;
                    report["downloadVerified"]=json!(true);report["output"]=json!(destination);
                }
            }
            if let Some(path)=value(args,"--report"){core::write_json(Path::new(&path),&report)?;}
            println!("{report}");
        },
        Some("verify-update")=>{
            let envelope=fs::read(required(args,"--manifest")?)?;
            let manifest=updater::verify_envelope(&envelope)?;
            if let Some(archive)=value(args,"--archive"){updater::verify_archive(&manifest,&fs::read(archive)?)?;}
            let report=json!({"passed":true,"version":manifest.version,"signatureVerified":true,"archiveVerified":value(args,"--archive").is_some()});
            if let Some(path)=value(args,"--report"){core::write_json(Path::new(&path),&report)?;}
            println!("{report}");
        },
        Some("--apply-update")=>{
            #[cfg(windows)] return update_install::apply(Path::new(args.get(1).context("缺少更新工作目录")?));
            #[cfg(not(windows))] bail!("原位更新需要 Windows");
        },
        Some("--help")|Some("help")=>println!("HotCoco Exporter {}\n双击打开图形界面；自动查找同级 .minecraft。\ndiscover [DIRECTORY] --report NEW.json\nscan DIRECTORY [--instance PACK_ID] --report NEW.json [--proxy HOST:PORT]\nexport DIRECTORY [--instance PACK_ID] --output NEW.zip --version VERSION [--own mods/FILE.jar] [--exclude PATH] [--report NEW.json]\nrestore PACKAGE --destination NEW_DIRECTORY [--provider modrinth|curseforge] [--report NEW.json]\nself-test --workdir NEW_DIRECTORY --report NEW.json\ncheck-update [--report NEW.json]\ndownload-update --output NEW.exe [--report NEW.json]\n--preview NEW.png",updater::VERSION),
        Some("--version")=>println!("HotCoco Exporter {}",updater::VERSION),
        Some(other)=>bail!("未知命令：{other}"),
    }
    Ok(())
}

fn self_test(work: &Path) -> Result<serde_json::Value> {
    use core::{Cancel, Manifest, Resource, Row};
    use hashes::Hashes;
    if work.exists() {
        bail!("自测需要新的工作目录");
    }
    fs::create_dir_all(work)?;
    let hashes = Hashes::of(b"abc")?;
    if hashes.sha256 != "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        || hashes.sha1 != "a9993e364706816aba3e25717850c26c9cd0d89d"
        || hashes.md5 != "900150983cd24fb0d6963f7d28e17f72"
    {
        bail!("系统哈希自测失败");
    }
    let root = work.join("publisher");
    fs::create_dir_all(root.join("mods"))?;
    let path = "mods/[自有资源] 测试 Mod.jar";
    let data = b"HotCoco embedded test; no executable classes.";
    fs::write(root.join(path), data)?;
    let mut row = Row {
        resource: Resource {
            resource_id: String::new(),
            path: path.into(),
            filename: "[自有资源] 测试 Mod.jar".into(),
            kind: "mod".into(),
            size: data.len() as u64,
            hashes: Hashes::of(data)?,
            sources: vec![],
            embedded: None,
        },
        selected: true,
        own: false,
        status: "待处理".into(),
        note: String::new(),
    };
    if core::manifest(&[row.clone()], "test").is_ok() {
        bail!("未知资源不能自动导出");
    }
    row.own = true;
    let manifest = core::manifest(&[row], "self-test")?;
    let package = work.join("self-test.zip");
    let cancel = Cancel::default();
    core::export(&root, &manifest, &package, &cancel)?;
    let destination = work.join("player");
    core::restore(
        &package,
        &destination,
        &net::Client::default(),
        None,
        &cancel,
        |_, _, _| {},
    )?;
    if fs::read(destination.join(path))? != data {
        bail!("自有资源或中文部署名还原失败");
    }
    let custom = destination.join("custom.jar");
    fs::write(&custom, b"keep")?;
    if core::restore(
        &package,
        &destination,
        &net::Client::default(),
        None,
        &cancel,
        |_, _, _| {},
    )
    .is_ok()
        || fs::read(&custom)? != b"keep"
    {
        bail!("已有目录保护失败");
    }
    let mut duplicate = manifest.clone();
    duplicate.files.push(duplicate.files[0].clone());
    if core::validate_manifest(&duplicate).is_ok() {
        bail!("重复资源检测失败");
    }
    let mut unsafe_path = manifest.clone();
    unsafe_path.files[0].path = "mods/../../outside.jar".into();
    if core::validate_manifest(&unsafe_path).is_ok() {
        bail!("路径校验失败");
    }
    let mut tampered = manifest.clone();
    tampered.files[0].hashes.sha256 = "0".repeat(64);
    let bad = work.join("bad.zip");
    {
        use std::io::Write;
        let mut archive = zip::ZipWriter::new(fs::File::create(&bad)?);
        let options = zip::write::SimpleFileOptions::default();
        archive.start_file(core::INDEX, options)?;
        archive.write_all(&serde_json::to_vec(&tampered)?)?;
        archive.start_file(manifest.files[0].embedded.as_ref().unwrap(), options)?;
        archive.write_all(data)?;
        archive.finish()?;
    }
    if core::restore(
        &bad,
        &work.join("bad-player"),
        &net::Client::default(),
        None,
        &cancel,
        |_, _, _| {},
    )
    .is_ok()
        || work.join("bad-player").join(path).exists()
    {
        bail!("错误哈希未阻止部署");
    }
    fs::write(root.join(path), b"changed")?;
    if core::export(&root, &manifest, &work.join("changed.zip"), &cancel).is_ok()
        || work.join("changed.zip").exists()
    {
        bail!("文件变化未阻止导出");
    }
    if hashes::fingerprint(b"a b\tc\nd\re") != hashes::fingerprint(b"abcde") {
        bail!("指纹过滤失败");
    }
    let _schema: Manifest = manifest;
    Ok(
        json!({"passed":true,"tests":["Windows CNG known hashes","unknown resources require explicit ownership","embedded resource roundtrip","Chinese alias preserved","existing directory protected","duplicate identity rejected","unsafe paths rejected","bad hash blocked before deployment","changed publisher file blocks export","CurseForge whitespace fingerprint"]}),
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = command(&args) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

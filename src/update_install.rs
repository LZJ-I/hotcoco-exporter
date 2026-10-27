//! The same EXE acts as a short-lived helper after the parent exits. No shell scripts or runtime.
use crate::{
    hashes::Hashes,
    updater::{self, Update},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::os::windows::{fs::MetadataExt, process::CommandExt};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::{Foundation::*, System::Threading::*};

const HIDDEN: u32 = 0x08000000;
const PREFIX: &str = ".hotcoco-update-";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    target: PathBuf,
    parent_pid: u32,
    original_sha256: String,
    envelope: String,
    proxy: Option<String>,
}

fn write_new(file: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}
fn plain(file: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(file)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("更新路径不能是链接");
    }
    Ok(())
}
fn hidden(file: &Path) -> Command {
    let mut command = Command::new(file);
    command
        .creation_flags(HIDDEN)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}
fn validate_plan(directory: &Path) -> Result<(PathBuf, Plan, updater::Manifest)> {
    plain(directory)?;
    let directory = directory.canonicalize()?;
    if !directory
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(PREFIX))
    {
        bail!("无效更新工作目录");
    }
    let plan_file = directory.join("plan.json");
    plain(&plan_file)?;
    if fs::metadata(&plan_file)?.len() > 128 * 1024 {
        bail!("更新计划过大");
    }
    let plan: Plan = serde_json::from_slice(&fs::read(plan_file)?)?;
    if plan.target.parent() != directory.parent()
        || plan
            .target
            .extension()
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("exe"))
        || plan.original_sha256.len() != 64
        || plan.parent_pid == 0
    {
        bail!("更新目标不在工具所在目录");
    }
    let manifest = updater::verify_envelope(plan.envelope.as_bytes())?;
    if updater::parse_version(&manifest.version)? < updater::parse_version(updater::VERSION)? {
        bail!("已拒绝工具降级");
    }
    Ok((directory, plan, manifest))
}
pub fn begin(update: &Update, bytes: &[u8], proxy: Option<String>) -> Result<PathBuf> {
    let manifest = updater::verify_envelope(&update.envelope)?;
    updater::verify_file(&manifest.executable, bytes)?;
    updater::validate_executable(bytes)?;
    let target = std::env::current_exe()?.canonicalize()?;
    plain(&target)?;
    let directory = target.parent().context("工具目录无效")?.join(format!(
        "{PREFIX}{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir(&directory).context("工具目录不可写，请把工具移到有写入权限的文件夹")?;
    let plan = Plan {
        target: target.clone(),
        parent_pid: std::process::id(),
        original_sha256: Hashes::sha256(&fs::read(&target)?)?,
        envelope: String::from_utf8(update.envelope.clone())?,
        proxy,
    };
    write_new(&directory.join("new.exe"), bytes)?;
    write_new(&directory.join("plan.json"), &serde_json::to_vec(&plan)?)?;
    fs::copy(&target, directory.join("helper.exe"))?;
    let mut child = hidden(&directory.join("helper.exe"))
        .arg("--apply-update")
        .arg(&directory)
        .spawn()
        .context("无法启动更新助手")?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if let Ok(ready) = fs::read_to_string(directory.join("ready")) {
            if ready.trim() == child.id().to_string() {
                return Ok(directory);
            }
        }
        if child.try_wait()?.is_some() {
            let detail = fs::read_to_string(directory.join("error.txt"))
                .unwrap_or_else(|_| "更新助手未能启动，原程序未改动".into());
            bail!("{detail}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    bail!("更新助手准备超时，原程序未改动")
}

struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn parent_process(plan: &Plan) -> Result<Process> {
    unsafe {
        let handle = OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            plan.parent_pid,
        );
        if handle.is_null() {
            bail!("原工具进程已退出，请重新检查更新");
        }
        let process = Process(handle);
        let mut buffer = vec![0u16; 32768];
        let mut size = buffer.len() as u32;
        if QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) == 0 {
            bail!("无法核对原工具进程");
        }
        let actual = PathBuf::from(String::from_utf16(&buffer[..size as usize])?).canonicalize()?;
        if actual != plan.target {
            bail!("更新目标与原工具进程不一致");
        }
        Ok(process)
    }
}
fn replace_files(directory: &Path, plan: &Plan) -> Result<()> {
    let backup = directory.join("previous.exe");
    if backup.exists() {
        bail!("更新已执行过");
    }
    plain(&plan.target)?;
    plain(&directory.join("new.exe"))?;
    if Hashes::sha256(&fs::read(&plan.target)?)? != plan.original_sha256 {
        bail!("原工具在下载期间发生变化，请重新更新");
    }
    fs::rename(&plan.target, &backup).context("无法替换原工具，请关闭其他正在运行的导出工具")?;
    if let Err(error) = fs::rename(directory.join("new.exe"), &plan.target) {
        fs::rename(&backup, &plan.target)
            .context("更新替换失败；旧程序保留在更新目录的 previous.exe")?;
        return Err(error.into());
    }
    Ok(())
}
fn launch_target(plan: &Plan, directory: &Path) -> Result<()> {
    let mut command = hidden(&plan.target);
    command.arg("--update-result").arg(directory);
    if let Some(proxy) = &plan.proxy {
        command.arg("--proxy").arg(proxy);
    }
    command.current_dir(plan.target.parent().unwrap()).spawn()?;
    Ok(())
}
fn rollback(directory: &Path, plan: &Plan) -> Result<()> {
    let backup = directory.join("previous.exe");
    if !backup.exists() {
        return Ok(());
    }
    plain(&backup)?;
    if Hashes::sha256(&fs::read(&backup)?)? != plan.original_sha256 {
        bail!("旧版本备份已变化，停止恢复");
    }
    if plan.target.exists() {
        let failed = directory.join("failed.exe");
        if failed.exists() {
            bail!("失败版本备份已存在，旧程序保留在 previous.exe");
        }
        fs::rename(&plan.target, failed)?;
    }
    fs::rename(backup, &plan.target)?;
    Ok(())
}
pub fn apply(directory: &Path) -> Result<()> {
    let result = (|| -> Result<()> {
        let (directory, plan, manifest) = validate_plan(directory)?;
        if std::env::current_exe()?.canonicalize()? != directory.join("helper.exe") {
            bail!("更新助手位置不匹配");
        }
        if Hashes::sha256(&fs::read(std::env::current_exe()?)?)? != plan.original_sha256 {
            bail!("更新助手与原版本不匹配");
        }
        let process = parent_process(&plan)?;
        let staged = directory.join("new.exe");
        plain(&staged)?;
        updater::verify_file(&manifest.executable, &fs::read(&staged)?)?;
        let version = hidden(&staged).arg("--version").output()?;
        if !version.status.success()
            || String::from_utf8_lossy(&version.stdout).trim()
                != format!("HotCoco Exporter {}", manifest.version)
        {
            bail!("新版程序启动校验未通过");
        }
        write_new(
            &directory.join("ready"),
            std::process::id().to_string().as_bytes(),
        )?;
        if unsafe { WaitForSingleObject(process.0, 60000) } != WAIT_OBJECT_0 {
            bail!("等待旧版本退出超时，原程序未改动");
        }
        drop(process);
        // Re-read and verify after waiting; no file replacement happens before this point.
        let installation = (|| -> Result<()> {
            updater::verify_file(&manifest.executable, &fs::read(&staged)?)?;
            replace_files(&directory, &plan)?;
            write_new(
                &directory.join("result.txt"),
                format!("已更新至 {}", manifest.version).as_bytes(),
            )?;
            launch_target(&plan, &directory)?;
            Ok(())
        })();
        if let Err(error) = installation {
            rollback(&directory, &plan)
                .context("自动恢复失败，旧版本保留在更新目录的 previous.exe")?;
            let _ = fs::write(
                directory.join("result.txt"),
                format!("更新未完成，已保留原版本：{error}"),
            );
            let _ = launch_target(&plan, &directory);
            return Err(error);
        }
        Ok(())
    })();
    if let Err(error) = &result {
        let _ = fs::write(directory.join("error.txt"), error.to_string());
    }
    result
}
pub fn completed(directory: &Path) -> Result<String> {
    let (directory, plan, _) = validate_plan(directory)?;
    if std::env::current_exe()?.canonicalize()? != plan.target {
        bail!("更新结果不属于当前工具");
    }
    let message = fs::read_to_string(directory.join("result.txt"))?;
    if message.len() > 4000 {
        bail!("无效更新结果");
    }
    // Never recursively delete an unvalidated path or a folder containing unexpected files.
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        plain(&entry.path())?;
        if !entry.file_type()?.is_file()
            || !matches!(
                entry.file_name().to_str(),
                Some(
                    "plan.json"
                        | "new.exe"
                        | "helper.exe"
                        | "previous.exe"
                        | "failed.exe"
                        | "ready"
                        | "result.txt"
                        | "error.txt"
                )
            )
        {
            bail!("更新目录包含其他文件，已保留备份");
        }
    }
    thread::spawn(move || {
        for _ in 0..30 {
            if fs::remove_file(directory.join("helper.exe")).is_ok() {
                for name in [
                    "plan.json",
                    "new.exe",
                    "previous.exe",
                    "failed.exe",
                    "ready",
                    "result.txt",
                    "error.txt",
                ] {
                    let _ = fs::remove_file(directory.join(name));
                }
                let _ = fs::remove_dir(&directory);
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PathBuf, PathBuf, Plan) {
        let root = std::env::temp_dir().join(format!(
            "hotcoco-update-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let directory = root.join(".hotcoco-update-test");
        fs::create_dir_all(&directory).unwrap();
        let target = root.join("[自定义] 资源导出工具.exe");
        fs::write(&target, b"old tool").unwrap();
        fs::write(directory.join("new.exe"), b"new tool").unwrap();
        fs::create_dir(root.join("mods")).unwrap();
        fs::write(root.join("mods/player-custom.jar"), b"keep").unwrap();
        let plan = Plan {
            target,
            parent_pid: 1,
            original_sha256: Hashes::sha256(b"old tool").unwrap(),
            envelope: String::new(),
            proxy: None,
        };
        (root, directory, plan)
    }
    fn clean(root: PathBuf) {
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("hotcoco-update-test-"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn replacement_preserves_local_filename_and_player_files() {
        let (root, directory, plan) = fixture();
        replace_files(&directory, &plan).unwrap();
        assert_eq!(fs::read(&plan.target).unwrap(), b"new tool");
        assert_eq!(
            fs::read(directory.join("previous.exe")).unwrap(),
            b"old tool"
        );
        assert_eq!(
            fs::read(root.join("mods/player-custom.jar")).unwrap(),
            b"keep"
        );
        clean(root);
    }
    #[test]
    fn failed_startup_can_restore_the_exact_old_program() {
        let (root, directory, plan) = fixture();
        replace_files(&directory, &plan).unwrap();
        rollback(&directory, &plan).unwrap();
        assert_eq!(fs::read(&plan.target).unwrap(), b"old tool");
        assert_eq!(fs::read(directory.join("failed.exe")).unwrap(), b"new tool");
        clean(root);
    }
    #[test]
    fn another_running_copy_blocks_swap_without_losing_original() {
        use std::os::windows::fs::OpenOptionsExt;
        let (root, directory, plan) = fixture();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&plan.target)
            .unwrap();
        assert!(replace_files(&directory, &plan).is_err());
        drop(lock);
        assert_eq!(fs::read(&plan.target).unwrap(), b"old tool");
        assert!(!directory.join("previous.exe").exists());
        clean(root);
    }
}

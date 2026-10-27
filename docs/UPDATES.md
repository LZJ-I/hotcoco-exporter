# 导出工具更新与发布

固定仓库为 `LZJ-I/hotcoco-exporter`，源码与 Release 均公开。更新器只访问公开资源，不内置任何 GitHub 令牌。GitHub Release 标签采用 `v主版本.次版本.修订号`，例如 `v0.3.0`；预发布、草稿和小于等于当前版本的版本不会提示更新。

每个新版 Release 必须同时上传：

- `hotcoco-exporter-windows-x64.zip`：只包含 `HotCoco-资源导出工具.exe`，ZIP 内保留中文原名。
- `hotcoco-exporter-update.json`：包含原始 JSON 载荷字符串及其 ECDSA P-256/SHA256 签名，签名为 64 字节 P1363 格式的十六进制字符串。
- `SHA256SUMS.txt`：供维护者和手动下载用户检查。

更新器校验程序内固定公钥的签名，再校验包、文件大小和哈希、ZIP 内路径及 x64 PE 类型。不会信任代理单独提供的校验值。Windows CNG 实现哈希和签名验证，不捆绑加密运行库。

## 发布操作

1. 修改 `Cargo.toml` 版本和更新日志，运行 `cargo test --offline`、Windows 目标测试及 `build-wsl.ps1`。Windows 文件版本、界面和命令行版本由 Cargo 版本统一生成。
2. 把签名私钥保存在仓库之外，并备份。设置 `HOTCOCO_EXPORTER_SIGNING_KEY_PATH` 指向该文件。私钥、GitHub 凭据不得提交到 Git；`src/update-public-key.hex` 是可以公开的 CNG 公钥。
3. 用已安装的 Node.js 运行开发脚本：

   ```powershell
   node scripts/package-release.mjs --exe target/x86_64-pc-windows-gnu/release/hotcoco-exporter.exe --out dist/0.3.0 --notes CHANGELOG.md
   ```

   此脚本只供维护者打包，不随 EXE 运行。它核对版本和签名密钥，生成 ZIP、签名更新清单与校验文件。输出文件已存在时拒绝覆盖。

4. 提交、push 源码后，在本仓库创建对应标签的 Release，上传这三个文件，确认附件齐全后公开。保持 Release 为正式版本；维护者不能只上传未签名 EXE，否则更新器会拒绝。
5. 用 EXE 的 `check-update --current-version 上一版本` 和 `download-update --current-version 上一版本 --output 全新路径.exe` 实测推荐加速通道。检查报告和下载 SHA256。

初始私钥保存在本次开发工作区的 `.deployment/exporter-update-signing/private-key.pem`，不在公开源码仓库中；后续打包必须继续使用同一密钥。更换公钥需要通过仍受旧密钥信任的版本迁移。

## 替换与恢复

用户点击“更新并重启”后，程序在当前 EXE 同级创建唯一的 `.hotcoco-update-*` 工作目录，并使用旧程序自身的一份副本作为短期助手。助手核对原进程路径、旧程序哈希、签名、新程序哈希和命令行版本；握手成功后界面退出，助手等待进程真正结束，再将旧 EXE 保留为 `previous.exe`，把新 EXE 移到原文件名并重启。

失败时恢复原 EXE；同目录的其他文件、游戏目录和玩家资源不参与更新。重启后的程序只清理经过路径和文件名单校验的此次更新工作目录。若系统阻止自动恢复，`previous.exe` 保留供人工恢复。

检查失败、网络中断、附件缺失、错误签名或哈希均不会触碰正在运行的 EXE。多个工具副本同时运行或目录不可写时，保持原程序并显示失败状态。

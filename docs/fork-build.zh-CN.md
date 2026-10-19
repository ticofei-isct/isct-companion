# Fork 修改、构建与提交上游

`Desktop preview` 在分支收到代码 push 时自动生成 Windows x64 安装包，
不需要发布签名密钥。GitHub 网页的 Upload files 提交也属于 push。
仅修改 Markdown 或 docs 目录不会触发构建。

## 上传修改与下载安装包

1. 在 fork 中选择 `build/auto-build` 分支，再上传修改好的源码，保留原目录结构。
2. 打开 Actions → Desktop preview → 对应提交的运行记录。
3. 等待 Windows installer 成功，在 Artifacts 下载
   `desktop-preview-windows-<完整提交 SHA>`，解压后运行 `.exe`。
   产物保存 14 天；每次提交的构建状态独立可查。

若 fork 提示工作流被禁用，先在 Actions 页面启用。
工作流合入默认分支后，也可以使用 Run workflow 手动选择分支构建。
这是未签名的测试安装包，Windows 可能显示安全提示。
构建流程执行前端类型检查和安装包构建；完整测试由现有 Platform checks
在 PR 中执行。不要把 `node_modules`、`target`、`dist` 或安装包提交进源码库。

## 本地提交

```sh
git switch build/auto-build
git add <修改的文件路径>
git commit -m "fix: describe the change"
git push origin build/auto-build
```

建议每个独立修复从最新的上游 main 创建新分支，以免多个修复混进一个 PR。
将本构建工作流合入 fork 的 main 后，新分支也会自动构建。

## 提交给原项目

在 GitHub 的 Contribute → Open pull request 中选择：

- base repository: `isct-companion/isct-companion`
- base: `main`
- head repository: `ticofei-isct/isct-companion`
- compare: 你的修改分支（当前为 `build/auto-build`）

检查 Files changed，只包含准备提交的修改，再创建 PR。
后续继续 push 同一个分支会自动更新该 PR。原项目维护者审核合并后，
修改才进入原项目；向 fork push 本身不会修改原项目。

macOS 安装包仍使用现有 Desktop release 手动工作流；不勾选 publish
即可只生成构建产物。正式发布沿用原有 Desktop release 流程。

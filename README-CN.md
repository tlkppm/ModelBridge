## ModelBridge

[English](./README.md)

ModelBridge 是面向 Cursor 的桌面模型网关。它在本机运行服务，将 Cursor Agent 连接到你配置的模型 API，并处理协议转换、工具调用协调、对话状态和请求诊断。

本仓库是基于 [leookun/cursor-byok](https://github.com/leookun/cursor-byok) 独立维护的分支。原始实现及其贡献者的归属保留在上游项目。ModelBridge 是本分支桌面应用的名称，不代表上游代码由本分支原创。

- 项目仓库：[tlkppm/ModelBridge](https://github.com/tlkppm/ModelBridge)
- 发行产物：[Releases](https://github.com/tlkppm/ModelBridge/releases)
- 构建执行记录：[GitHub Actions](https://github.com/tlkppm/ModelBridge/actions)
- 源码许可证：[MIT](./LICENSE)

## 本分支的范围

- 使用 ModelBridge 应用名称，并独立维护桌面版本。
- 桌面导航聚焦数据概览、调用详细、Cursor 模型配置和系统设置。
- 从桌面界面移除内置使用教学和插件配置页面；后端插件运行机制与这些页面相互独立。
- 自动更新使用本仓库的发行地址，并使用专属公钥验证更新签名。
- 保留上游的协议适配、模型服务接入和本地对话处理基础。

本分支与上游独立维护。发行版本、变更和反馈应以本仓库为准，不应将上游发行页面视为本分支的产物来源。ModelBridge 与 Cursor 及其开发者没有关联，也未获得其认可。

## 核心能力

- 自定义模型服务地址、模型标识、凭据、请求头和生成参数。
- 接入兼容 OpenAI Responses API、OpenAI Chat Completions API 和 Anthropic Messages API 的模型服务。
- 管理模型配置、测试连接并检查服务商响应。
- 处理 Cursor Agent 请求、工具调用、可复用的 Skills，以及模型上下文协议（Model Context Protocol，MCP）工具接入。
- 查看调用详细、Token 用量、缓存指标、延迟和对话统计。
- 管理桌面应用生命周期设置，并验证自动更新签名。

实际兼容性取决于 Cursor 版本、模型服务商、模型能力和请求协议。支持某种协议不代表所有服务商都会一致实现该协议的全部能力。

## 桌面发行

发行工作流配置了以下目标：

- Windows x86_64。
- Linux x86_64。
- macOS Apple Silicon，使用 aarch64 架构。
- macOS Intel，使用 x86_64 架构。

请以本仓库 [Releases 中实际存在的附件](https://github.com/tlkppm/ModelBridge/releases) 为准，确认哪些安装包或程序已经构建并发布。源码压缩包、版本标签或工作流文件本身都不等于已经生成可执行程序。

发行流水线先将各平台产物上传到草稿发行版，全部平台任务成功后才正式发布。构建进度和失败原因应查看 [Actions 执行记录](https://github.com/tlkppm/ModelBridge/actions)，不能仅凭标签存在判断构建已经运行。

自动更新签名用于通过本分支配置的公钥验证更新包。它与 Windows 可执行文件签名、Apple Developer 签名及 Apple 公证相互独立；存在更新签名不代表已完成操作系统代码签名或公证。

## 本地数据与外部服务

网关运行在你的设备上。运行时 API 凭据、模型配置、应用设置和对话记录保存在本机。

模型请求仍会将必要的对话上下文和工具结果发送给你配置的服务商。其他已启用的集成也可能访问外部服务。使用本地网关不等于让远程模型离线运行。

模型服务的计费和隐私政策由对应服务商决定。开源应用不附带模型 API 额度，也不保证能够使用 Cursor 的付费服务。不要将凭据、私钥、本地数据库或未脱敏的请求日志提交到公开仓库或反馈内容中。

## 开发说明

桌面前端采用 React、TypeScript 和 Tauri 2。本地服务使用 Rust 实现，并通过 SQLite 持久化数据。

前端与桌面命令定义在 [`apps/desktop/package.json`](./apps/desktop/package.json)。在该目录执行 `npm run check` 会运行前端类型检查和前端生产构建；`npm run tauri:build` 则使用当前平台的构建依赖编译原生桌面应用。

Rust 工作区定义在 [`Cargo.toml`](./Cargo.toml)。格式检查、lint 和测试任务见 [CI 工作流](./.github/workflows/ci.yml)。平台依赖、发行目标、签名输入和发布阶段见 [发行工作流](./.github/workflows/release.yml)。

前端构建成功不等于原生应用编译成功，某个平台编译成功也不能替代在该操作系统上的实际运行测试。

## 贡献与反馈

ModelBridge 的变更应提交到本分支的 [Pull Requests](https://github.com/tlkppm/ModelBridge/pulls)。描述问题时，请提供应用版本、操作系统、服务商协议、复现步骤和已脱敏的错误信息。

上游项目的路线图、社区群组、推广链接和发行统计不作为本分支自身的资源展示。

## 来源与许可证

ModelBridge 派生自 leookun 及其贡献者开发的 [cursor-byok](https://github.com/leookun/cursor-byok)。原始版权声明与 MIT 许可证保留在 [`LICENSE`](./LICENSE) 中。

与 Semble 派生搜索实现有关的其他来源说明保留在 [`UPSTREAM.md`](./UPSTREAM.md) 中。

## ModelBridge

[简体中文](./README-CN.md)

ModelBridge is a desktop model gateway for Cursor. It runs a local service that connects Cursor Agent to model APIs you configure, with protocol adaptation, tool-call coordination, conversation state, and request diagnostics.

This repository is an independently maintained fork of [leookun/cursor-byok](https://github.com/leookun/cursor-byok). The original implementation and its contributors remain credited to the upstream project. ModelBridge is the name of this fork's desktop application, not a claim of original authorship of the upstream code.

- Repository: [tlkppm/ModelBridge](https://github.com/tlkppm/ModelBridge)
- Release assets: [Releases](https://github.com/tlkppm/ModelBridge/releases)
- Build execution records: [GitHub Actions](https://github.com/tlkppm/ModelBridge/actions)
- Mainland China community: QQ group **Plus/Pro 分享群**, group number `554549065`.
- Source license: [MIT](./LICENSE)

## Scope of this fork

- Uses the ModelBridge application name and maintains its own desktop versions.
- Focuses desktop navigation on the dashboard, request details, Cursor model configuration, system settings, and a dedicated About page with the application version, repository link, and QQ community information.
- Removes the built-in tutorial and plugin configuration pages from the desktop interface. The underlying plugin runtime is separate from those pages.
- Uses this repository's release feeds and a dedicated public key for automatic-update signature verification.
- Retains the upstream protocol adaptation, provider integration, and local conversation-processing foundation.

This fork is maintained separately from the upstream project. Its releases, changes, and support should be evaluated against this repository rather than the upstream release page. ModelBridge is not affiliated with or endorsed by Cursor or its developers.

## Core capabilities

- Custom model endpoints, model identifiers, credentials, request headers, and generation parameters.
- OpenAI Responses API, OpenAI Chat Completions API, and Anthropic Messages API compatible providers.
- Model configuration management, connection testing, and inspection of provider responses.
- Cursor Agent request handling, tool calls, reusable Skills, and Model Context Protocol (MCP) tool integration.
- Request details, token usage, cache metrics, latency, and conversation statistics.
- Desktop application lifecycle settings and signed automatic-update verification.

Compatibility depends on the Cursor version, the selected provider, the model's capabilities, and the API protocol. Support for a protocol does not guarantee that every provider implements every feature consistently.

## Desktop releases

The release workflow is configured for these targets:

- Windows x86_64.
- Linux x86_64.
- macOS Apple Silicon, using aarch64.
- macOS Intel, using x86_64.

Check this repository's [release assets](https://github.com/tlkppm/ModelBridge/releases) for the packages that have actually been built and published. A source archive, version tag, or workflow file alone is not a completed application package.

The release pipeline builds platform packages into a draft release and publishes the release only after all configured platform jobs succeed. Build progress and failures belong to the [Actions execution records](https://github.com/tlkppm/ModelBridge/actions), not the existence of a tag.

Automatic-update signatures verify update packages against this fork's configured public key. They are separate from Windows executable signing and Apple Developer signing or notarization; an update signature does not establish operating-system signing or notarization.

## Local data and external services

The gateway runs on your machine. Runtime API credentials, model configuration, application settings, and conversation records are stored locally.

Model requests still send the required conversation context and tool results to the configured provider. Other enabled integrations may also use external services. Running a local gateway does not make a remote model run offline.

Provider billing and privacy policies remain the provider's responsibility. The open-source application does not include model API credits or guarantee access to Cursor's paid services. Keep credentials, private keys, local databases, and unredacted request logs out of public commits and feedback.

## Development

The desktop frontend uses React and TypeScript with Tauri 2. The local service is implemented in Rust and uses SQLite for persistence.

The frontend and desktop commands are defined in [`apps/desktop/package.json`](./apps/desktop/package.json). From that directory, `npm run check` runs frontend type checks and the production frontend build, while `npm run tauri:build` builds the native desktop application with the current platform's build dependencies.

The Rust workspace is defined in [`Cargo.toml`](./Cargo.toml). Formatting, lint, and test jobs are described in the [CI workflow](./.github/workflows/ci.yml). Platform dependencies, release targets, signing inputs, and publication stages are defined in the [release workflow](./.github/workflows/release.yml).

A successful frontend build is not a native application build. A successful platform build also does not replace runtime testing on that operating system.

## Community and contributions

The QQ group **Plus/Pro 分享群** is a community discussion channel for users in mainland China. It welcomes discussion of ModelBridge model integration, application usage, and release feedback.

- QQ group number: `554549065`.
- Joining: search for the group number in QQ; admission is subject to the group's settings and approval process.
- Technical feedback: include the application version, operating system, provider protocol, reproduction steps, and sanitized errors.

This is a community channel, separate from official Cursor, OpenAI, or upstream project support. Keep API keys, passwords, login verification codes, session credentials, and private conversation data out of shared messages and logs.

Code changes and reviews should target this fork's [pull requests](https://github.com/tlkppm/ModelBridge/pulls).

## Attribution and license

ModelBridge is derived from [cursor-byok](https://github.com/leookun/cursor-byok) by leookun and its contributors. The original copyright notice and MIT license are retained in [`LICENSE`](./LICENSE).

Additional provenance for the Semble-derived search implementation is recorded in [`UPSTREAM.md`](./UPSTREAM.md).

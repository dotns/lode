# lode 文档

[English](README.md) · **中文**

多数文档提供两个单一语言版本:`X.md`(英文)与 `X.zh-CN.md`(中文),顶部互相链接。

| 文档 | English | 中文 |
|---|---|---|
| 项目概览 / 快速上手 | [`../README.md`](../README.md) | [`../README.zh-CN.md`](../README.zh-CN.md) |
| 集成 —— 配置 → 运行 → 发布 | [`integration.md`](integration.md) | [`integration.zh-CN.md`](integration.zh-CN.md) |
| 架构文档(权威规范) | [`architecture.md`](architecture.md) | [`architecture.zh-CN.md`](architecture.zh-CN.md) |
| 离线本地测试(`lode-cli seed`) | [`dev-local-testing.md`](dev-local-testing.md) | [`dev-local-testing.zh-CN.md`](dev-local-testing.zh-CN.md) |
| 来源适配器 —— 签名规范(权威)、manifest 形状、GitHub / 原生发布 | [`source-adapters.md`](source-adapters.md) | [`source-adapters.zh-CN.md`](source-adapters.zh-CN.md) |

参考(语言中立):

- [`lode.example.toml`](lode.example.toml) —— 完整、分段的 `lode.toml`。
- [`manifest.example.json`](manifest.example.json) —— 最全的 `lode/v1` manifest。
- [`recipes/`](recipes/) —— Bun / Node / Deno 应用在 `[runtime]` 下运行的 `lode.toml` 配方。

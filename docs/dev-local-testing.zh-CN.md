# 不依赖发布源的本地测试(`lode-cli seed`)

[English](dev-local-testing.md) · **中文**

开发时通常希望**不搭建** manifest/GitHub 源、不配签名密钥、也完全不联网,就把一个二进制
跑在 lode 下。lode 天生支持这点:**只有在本地没有任何可用版本时**才会去联系远程;任何已在
磁盘上的版本,都直接从它的 `.lode.json` marker 启动(设计 §15)—— 不下载、不验签、不联网。

`lode-cli seed` 直接把一个本地二进制(或压缩包)装成一个版本,方便迭代。

## 速览

```bash
# 把 ./myapp 作为一个版本装进临时 data 目录,并激活
lode-cli --data-dir /tmp/lode-dev seed ./myapp --version 1.0.0

# 运行它 —— 裸 lode,完全离线,无需配源
lode --data-dir /tmp/lode-dev
```

`seed` 默认激活该版本;在全新目录里还会 scaffold 一份**无源 `lode.toml`**(这样裸 `lode`
启动时没有源)。就这些。

> **`lode` vs `lode-cli`。** 裸 `lode` **就是**那个监督服务 —— **没有 `serve` 子命令**;
> loader 的 argv 被故意留空,好让 `lode <参数>` 透明 exec 直通到 app。管理/开发类子命令
> (`seed`、`versions`、`status`、`rollback`、`restart`、`update`)在 **`lode-cli`** 这个名字
> 下 —— 它是指向同一二进制的符号链接:`ln -s lode target/debug/lode-cli`。

## `lode-cli seed`

```
lode-cli [--data-dir DIR] [--app NAME] seed <APP_BIN> [选项]

  <APP_BIN>        本地可执行文件,或 .tar.gz / .zip / .gz 压缩包
  --version VER    版本 id(默认 0.0.0-dev),用作 versions/<VER> 的键。用 semver,
                   以便 rollback / 禁降级 floor 的排序正确
  --entry NAME     版本目录内入口文件名(默认从文件推导 —— 裸二进制取其 basename,
                   压缩包取与 app 同名、位于归档根目录的文件)
  --no-activate    只装进 versions/,不切 current / 不写 state.json
```

它执行的 staging + 原子激活与真实 install 完全一致,**只是省掉**下载、sha256 完整性校验
和签名校验 —— 字节由你自己提供(可信)。源文件是**拷贝**,不消费。

### 从本地压缩包重建版本目录(用发布 `.tar.gz`/`.zip` 重建)

把发布会用的同一个压缩包交给它,它就把 `versions/<VER>/` 下整棵版本目录树重建出来。`format`
由扩展名判断;用 `--entry` 给出归档内可执行文件的路径(归档无法自动判定哪个是"主程序" ——
不给 `--entry` 时,会在归档根目录找与 `--app` 同名的文件):

```bash
# myapp-1.0.0.tar.gz 里含  bin/myapp  (以及其它文件)
lode-cli --data-dir /tmp/lode-dev --app myapp seed ./myapp-1.0.0.tar.gz \
    --version 1.0.0 --entry bin/myapp
```

→ 重建结果,与真实 install 逐字节一致:

```
versions/1.0.0/
├── bin/myapp           # 解包并 +x
├── lib/…               # 归档其余内容,完整保留
└── .lode.json          # { "version": "1.0.0", "entry": "bin/myapp", "format": "tar.gz" }
```

## 写出了什么

```
<data-dir>/
├── lode.toml                      # 若缺失则 scaffold 一份无源配置(policy=off)
├── versions/
│   └── 1.0.0/
│       ├── myapp                  # 你的二进制(入口),+x
│       └── .lode.json             # { "version": "1.0.0", "entry": "myapp", "format": "raw" }
├── current -> versions/1.0.0      # (除非 --no-activate)相对软链
└── state.json                     # (除非 --no-activate){ "current": "1.0.0", "last_good": "1.0.0" }
```

## 为什么这样能离线运行

lode 只在两个地方联网,这里都被避开了:

1. **启动 bootstrap** —— 仅当 data 目录里**一个版本都没装**时。`seed` 之后,启动会在本地解析
   出 seeded 版本并从 marker 启动,bootstrap 永不触发。
2. **周期更新检查** —— 当 `[update].policy = "off"`(scaffold 出的配置就是)或设了 `pin` 时
   **完全不调度**:supervisor 不安排检查,也就永不 fetch。(即便 `check`/`auto` 下,fetch 失败
   也是 best-effort —— 记日志后忽略,远程坏了不会影响正在跑的版本。)

无源配置是合法的:既不设 `[update].manifest` 也不设 `[update].github` 时,lode 只记一条
"no update source configured" 日志,然后跑已装好的东西。

## 多版本(rollback / 降级测试)

对同一个 data 目录每个版本各 seed 一次,再用 `lode-cli` 驱动:

```bash
lode-cli --data-dir /tmp/lode-dev seed ./myapp-v1 --version 1.0.0
lode-cli --data-dir /tmp/lode-dev seed ./myapp-v2 --version 1.1.0 --no-activate

lode-cli --data-dir /tmp/lode-dev versions          # 两个都列出;* 标记 current
lode-cli --data-dir /tmp/lode-dev rollback --version 1.0.0   # 刻意降级,纯本地,不 fetch
```

## 注意

- **仅供开发/测试。** `seed` 跳过完整性与签名校验,因为没有下载。生产安装走 `lode-cli update`
  (或 supervisor 的 bootstrap),按 `[trust].require_signature` 验证。
- 想离线测试**真实**更新流程,把 `[update].manifest` 指向本地 `http://127.0.0.1` 服务器
  (见 `tests/` 下的 e2e harness),而不是 seed。
- 空 data 目录首次启动是唯一**必须** fetch 的场景(还没有可跑的东西)。气隙环境首启,要么
  `seed` 一个版本,要么拷一个曾经在线填充过的 data 目录。

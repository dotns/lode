# 把应用接入 lode

[English](integration.md) · **中文**

接入涉及三个文件,各有唯一归属:

| 文件 | 位置 | 谁写 | 作用 |
|---|---|---|---|
| **`lode.toml`** | 本地 | 运维 | lode 如何拉取并运行你的应用 |
| **`state.json`** | 本地(`$LODE_DIR`) | lode **与** 应用 | 运行时通信(状态 ↔ 请求) |
| **发布源** | 远程 | 发布方 | 已签名的资产清单 —— 原生 `manifest.json` **或** GitHub Releases |

下面三步 —— **配置 → 运行 → 发布** —— 就是完整的接入。运维点名要装哪个资产
(`[update].asset`),该文件名在两个源里都是选择 key。完整签名规范见
[source-adapters.zh-CN.md](source-adapters.zh-CN.md);穷尽字段见
[`lode.example.toml`](lode.example.toml) 与 [`manifest.example.json`](manifest.example.json);
深入设计见[架构文档](architecture.zh-CN.md)。

---

## 1. 配置 lode(`lode.toml`)

运维的文件:*如何拉取与运行你的应用*。应用从不写它。优先级
`CLI > 环境变量(LODE_*) > lode.toml > 默认值`;默认 lode 读取 `/srv/lode/lode.toml`
(用 `LODE_DIR` 改基目录),首次运行会在那里生成一份起始配置。

```toml
[global]
app      = "myapp"          # 必须与 manifest 的 "name" 一致
dir = "/srv/lode"      # 存放 lode.toml + versions/ + state.json + lode.pid + runtime/

[update]
github   = "owner/myapp"                                        # GitHub Releases ……
# manifest = "https://releases.example.com/myapp/manifest.json" # …… 或原生 manifest(二选一)
asset    = "myapp-linux-x64.tar.gz"   # 本机要的资产文件名(选择 key)
channel  = "stable"         # github:stable=/releases/latest,否则最新 prerelease ; 原生:通道名
policy   = "auto"           # off | check | auto
# pin    = "1.4.2"          # 锁定版本(关闭自动更新)

[trust]
require_signature = "enforce"                       # off | auto | enforce
trusted_keys = ["<key_id>:<base64-公钥>"]           # 来自 `lode-cli keygen`

[command]
run     = "./myapp"         # 启动应用的字面命令(cwd=版本目录;manifest 资产可覆盖)
exec    = "./myapp"         # `lode <args>` 透传的字面基准命令
# workdir = "{dir}"         # 可选;省略即版本目录(默认)。需固定部署目录(如读 .env)可写绝对路径

[supervise]
readiness    = "state"      # none | state(仅当应用自报就绪后才提交该版本)
health_grace = 15           # 新版本须存活满的秒数才算 good(否则回滚)
stop_timeout = 10           # SIGKILL 前的优雅停止窗口
restart      = "on-failure" # on-failure(默认,keep-alive:重试后暂停)| always | off(镜像子进程)
```

常见形态:

- **自带二进制:** `run = "./myapp serve"`、`exec = "./myapp"`(或省略 `[command]`,改为在 manifest 资产里发布 `run`/`exec`)。
- **脚本 + 运行时:** `run = "bun run"`、`exec = "bun"`,再加 `[runtime]` 段,PATH 上没有 `bun` 时下载它——下载后缓存复用,可用 `version` 锁版本;注意运行时下载**不做签名校验**。
- **私有源:** 加 `[http].headers = ["Authorization: Bearer ${TOKEN}"]` —— 随 manifest 与产物请求发送,展开 `${ENV}`。

全部选项及 `[runtime]`/`[signals]`/`restart_*` 见 [`lode.example.toml`](lode.example.toml)。

---

## 2. 应用契约(`state.json`)

*你的应用*要实现的部分。任意语言 —— 读写一个 JSON 文件 + 处理 `SIGTERM`。

> **优先用 SDK。** [`../sdks`](../sdks) 下有单文件客户端(TypeScript / Go / Rust),封装了整个契约 —— 读状态;请求升级 / 重启 / 回退;维护用 `hold`/`release`;上报就绪;`watch` 监听 lode 的通知 —— 且写入都是原子、经 `state.json.lock` 串行化的。下面的原始契约正是它们的实现(也是你移植到其它语言所需的全部)。

**lode 注入的环境变量:** `LODE_ACTIVE_VERSION`(当前版本)、`LODE_DIR`(lode 自己的目录 ——
`state.json` 在 `$LODE_DIR/state.json`)、`LODE_WORKDIR`(app 的运行目录,即其 cwd)、
`LODE_INSTANCE`(本次启动唯一号 —— 写入 `state.ready`)、`LODE_READINESS`(`none`|`state`)。
宿主环境(如 `PORT`)原样透传;内部 `LODE_*` 已剥离。operator 还可用 `[env]` 表追加变量——
它们是**默认值**:同名的宿主 env(如部署时 `-e PORT`)会覆盖它们,而 lode 注入的变量始终最高。
`LODE_DIR`/`LODE_WORKDIR` 与 app 自有的 `ROOT_DIR`/`DATA_DIR` 的关系见[数据目录与持久化](#数据目录与持久化)。

**state.json** —— lode 写状态、应用写请求,字段不重叠:

```jsonc
{
  // lode 写(应用读):
  "current": "1.4.2", "last_good": "1.4.2", "available": "1.5.0",
  "status": "running",        // starting|running|held|updating|rolling-back|stopping|stopped|error
  "pid": 12345, "last_check": "…", "last_error": null,
  "config_generation": 0,     // 运行中编辑 lode.toml 时 lode 递增它 => 重启才生效
  // 应用写(请求 / 就绪):
  "target": null,             // 某版本或 "latest" => 请求升/降级
  "restart_nonce": 0,          // 递增 => 重启当前版本
  "hold": false,              // 置 true => lode 不再(重新)启动进程(维护)=> 状态 "held"
  "ready": null               // 写成 LODE_INSTANCE => "我能服务了"
}
```

实现以下契约(除 `SIGTERM` 外均可选,但推荐):

- **优雅停止(必需):** 收到 `SIGTERM` 后排空并在 `stop_timeout` 内 `exit(0)`,否则被 `SIGKILL`。lode 会先置 `status = updating|stopping` 供你区分。
  ```ts
  process.on("SIGTERM", async () => { await drain(); process.exit(0) })
  ```
- **就绪(当 `readiness = "state"`):** 真正能服务后,原子写 `state.ready = LODE_INSTANCE`(临时文件 + rename,保留 lode 的字段)。在此之前 lode 不提交该版本(零停机模式下也不停旧实例);超过 `ready_timeout` 未就绪 → 回滚。
  - **相位"准备"握手(选用制):** 用 `state.ready = "{LODE_INSTANCE}-0"`(而非裸 token)报就绪即选用。更新时 lode 写 `state.ready = "{LODE_INSTANCE}-1"` 提示你准备;排空/落盘后写 `"{LODE_INSTANCE}-2"` 应答,切换才开始。切换默认由 app 自定节奏(`prepare_timeout = 0`);非 0 的 `[supervise].prepare_timeout`(秒)在你始终不应答时强制切换。机制详见[架构文档](architecture.zh-CN.md) §8。
- **健康:** 启动失败要 `exit(非0)`。新版本若在 `health_grace` 内退出,回滚到上一个 good(单次触发)。
- **自报版本**(如 `GET /version`),与 `LODE_ACTIVE_VERSION` 一致。
- **请求更新/重启(可选):** 原子改写 `state.json` —— 设 `target`(版本或 `"latest"`)或递增 `restart_nonce`。lode 轮询文件 mtime(~1s)并执行;文件本身即通知。
- **阻止(重新)启动(可选):** 设 `hold = true` 告诉 lode **不要**(重新)启动进程 —— 用于必须在 app 起来之前完成的计划维护(如需要 CLI 介入的 DB 迁移)。lode 报告 `status = "held"` 并等待 —— 开机时、子进程退出后、以及 `restart_nonce`/`target` 请求期间都不启动 —— 直到你设 `hold = false`。hold 只挡"启动",不杀正在运行的子进程:想为维护停掉 app,自己先 `exit(0)`(lode 随即转入 held 而非重拉)。运维也可同样方式驱动(`hold` 就是 `state.json` 的一个字段)。
- **运行中应用 `lode.toml`/`[env]` 改动(可选):** lode **绝不**因配置编辑自动重启(运行中的 app 不被打扰)。`lode.toml` 被编辑时,lode **递增 `config_generation`** 通知你;你自定时机递增 `restart_nonce` 来应用 —— 那次重启会**重读 `lode.toml`**(新 `[env]`/配置生效)。想响应运维改动就监听 `config_generation`。(宿主进程 env —— `-e`/k8s —— 仍需重启 lode 自身。)

> 推荐:使用 [SDK](../sdks)(并参考 [`../examples`](../examples),它们都经由 SDK 接入)。一对零依赖、不用 SDK 的 Rust + Bun 手写示例见 [`../tests/apps`](../tests/apps),作为从零实现的参考。

---

## 3. 发布发布源

lode 解析 **channel → version → asset**,校验后安装/运行。每台主机装哪个资产由**文件名**
(`[update].asset`)决定,每个资产都带一个对规范消息
`lode.artifact.v1\n{name}\n{version}\n{sha256}\n{run}\n{exec}`(UTF-8、`\n` 分隔、无结尾换行;`run`/`exec` 缺省为空字符串)的 ed25519
签名。`name` 是资产文件名。完整规范(含原生 manifest 形状与字段表)见
[source-adapters.zh-CN.md](source-adapters.zh-CN.md)。

打包 + 签名是**发布方**的事,可在任意 CI 完成。`lode-cli` 是参考实现;任何产出相同签名的
ed25519 工具效果一致。

### 密钥(一次性)

`lode-cli keygen` 打印 `key_id`、`trusted_keys` 条目(`<key_id>:<base64>`,交给运维)、以及
保密种子 —— 离线保存。

### GitHub Releases(`github = "owner/repo"`)

把这份 workflow 放进**你的应用**仓库。它构建你的资产,并**仅当配置了签名密钥时**才对每个
资产签名、把签名作为资产 `label` 上传。没有 key 时上传未签名版本,所以在你采用签名之前也能用。

```yaml
# .github/workflows/release.yml —— 为 lode 发布你的应用资产
on:
  release:
    types: [published]      # 建 release(UI 或 `gh release create`);本 workflow 附加资产
permissions:
  contents: write
jobs:
  release:
    runs-on: ubuntu-latest
    env:
      GH_TOKEN: ${{ github.token }}
      TAG: ${{ github.event.release.tag_name }}
      LODE_SIGNING_KEY: ${{ secrets.LODE_SIGNING_KEY }}   # 可选 —— 设了才启用签名
    steps:
      - uses: actions/checkout@v4

      - name: Build assets                # -> dist/<app>-<os>-<arch>.<ext>(由你提供)
        run: ./build.sh "$TAG"

      - name: Publish(仅当配置了 key 才签)
        run: |
          set -euo pipefail
          if [ -n "${LODE_SIGNING_KEY:-}" ]; then
            curl -fsSL https://github.com/dotns/lode/releases/latest/download/lode-linux-x64.tar.gz \
              | tar -xz lode lode-cli                 # 取 lode-cli 用于签名
          fi
          for f in dist/*; do
            if [ -n "${LODE_SIGNING_KEY:-}" ]; then
              sig=$(./lode-cli sign "$f" --version "$TAG" --key-env LODE_SIGNING_KEY)
              gh release upload "$TAG" "$f#$sig" --clobber     # label = 签名
            else
              gh release upload "$TAG" "$f" --clobber          # 未签名
            fi
          done
```

- **启用签名:** `lode-cli keygen` 一次;把保密种子放进仓库的 `LODE_SIGNING_KEY` secret
  (离线另存一份),并把公开的 `trusted_keys` 条目交给运维。没设 secret → 资产以未签名上传
  (在你采用签名前没问题;签名分支不会执行)。
- lode 选 `name` 等于运维 `[update].asset` 的资产;`sha256` 取自资产 `digest`(对字节复验),
  `version` 取自 tag。`channel = stable` → `/releases/latest`;其它 channel → 最新非草稿
  prerelease;`pin` → 指定 tag。无需 `manifest.json` 资源。私有库:token 放 `[http].headers`。
- 资产命名 `<app>-<os>-<arch>.<ext>`;每台主机的运维把 `[update].asset` 设为本机对应的确切
  文件名。

### 原生 manifest(`manifest = "https://.../manifest.json"`)

托管一份 `lode/v1` manifest,其每版 `assets[]` 按 `name`,外加资产托管在任意 HTTPS URL:

```bash
lode-cli manifest "$f" --version 1.5.0 --url "$URL" \
    --run ./myapp --exec ./myapp \
    --key private.key --into manifest.json   # 按 name upsert 资产,设 channels.latest;--run/--exec 可选
lode-cli manifest-sign --into manifest.json --key private.key   # 可选:对目录做防篡改证据
```

manifest 形状 + 逐资产字段表见 [source-adapters.zh-CN.md §6](source-adapters.zh-CN.md)。
`manifest-sign` **可选**(present 才验的防篡改证据);`channels.<c>.latest` 的回滚由客户端
禁降级 floor 拦截,`pin` 则彻底不再信任指针。

### 签名模型(两源通用)

- artifact 签名绑定 **`name`(文件名)/ `version` / `sha256` / `run` / `exec`**。`format` 从文件名后缀推导(`.tar.gz`/`.tgz` → tar.gz、`.gz` → gz、`.zip` → zip、否则 raw)。`run`/`exec` 在存在时绑入签名(缺省为空字符串)——在 `require_signature=auto`(有密钥)或 `enforce` 下,被篡改的目录无法注入恶意启动命令。
- `require_signature = enforce` 下,每个安装的资产都必须带有效签名(github:`label`;native:
  `sig` 字段或 `.sig` sidecar)。`auto` 一旦配置了任一受信公钥即 fail-closed;无公钥时安装为
  **UNVERIFIED** 并告警。

### 清单

- [ ] 每台主机的 `[update].asset` 写明本平台对应的确切资产文件名。
- [ ] `sha256` 针对原始文件;`sig` 针对 `name/version/sha256`,`key_id` 受信。
- [ ] github:签名设为资产 **`label`**。native:`sig` 内嵌或 `.sig` sidecar,且最后一次改目录后重新 `manifest-sign`。
- [ ] `channels.<c>.latest` 指向真实版本(native),或 tag/latest 可解析(github)。
- [ ] 私钥离线;运维只持公开的 `trusted_keys` 并设 `require_signature = enforce`。

---

## 数据目录与持久化

两类目录,各自命名,互不混淆。

**lode 提供** —— lode 注入,app 读取:

| 环境变量 | 是什么 | 生命周期 |
|---|---|---|
| `LODE_DIR` | lode 自己的目录 —— `lode.toml`、`versions/<ver>/`、`state.json`(及 `.lock`)、`lode.pid`、`runtime/`、`downloads/` | **持久** —— 挂卷(`-v lode-data:/srv/lode`) |
| `LODE_WORKDIR` | app 在 lode 下的运行目录(其 cwd;默认 = 当前版本目录) | **随版本轮换** —— 被 `keep_versions` 回收 |

(`lode` 二进制本身随镜像发布、不存状态。operator 配置:`[global].dir` / `--dir` 设 `LODE_DIR`;子进程 cwd 默认是版本目录,可在 `lode.toml` 的 `[command].workdir` 覆盖(无 CLI flag)—— lode 随后把解析出的目录注入为 `LODE_WORKDIR`。)

**app 自行实现** —— app 自己的目录约定,让*同一个二进制不依赖 lode 也能用*。**lode 从不读取或设置它们 —— 原样透传给子进程**(用宿主 env / `-e` / `[env]` 表设置);下面的回退完全是你的 app(或 SDK)的事:

| 环境变量 | 是什么 |
|---|---|
| `ROOT_DIR` | app 的根/运行目录 —— 独立运行时唯一需要设的 |
| `DATA_DIR` | app 的持久化数据目录 |

按 **`DATA_DIR` > `LODE_DIR` > `ROOT_DIR`** 解析你的数据目录,这样你*只需*设 `ROOT_DIR`:

- 独立运行:只设 `ROOT_DIR`,其余都回退到它;
- 在 lode 下:自动用 `LODE_DIR`(持久卷);
- 设 `DATA_DIR` 显式覆盖(如另挂一个卷)。

```ts
const dataDir = process.env.DATA_DIR ?? process.env.LODE_DIR ?? process.env.ROOT_DIR;
```

SDK 直接给出:`dataDir()`(上面的回退链),外加 `rootDir()` / `lodeDir()` / `workdir()`。

> **别把持久数据写进 cwd。** `LODE_WORKDIR` 默认是每版本目录,lode 每次更新都替换它、并按 `keep_versions` 回收。app 要保留的状态放到你解析出的 `DATA_DIR`(持久位置),绝不要放版本目录。

# lode 源适配器 —— 签名与实施指南

[English](source-adapters.md) · **中文**

lode 从且仅从一个源拉取更新:**native** 清单 URL,或 **GitHub Releases** 仓库。两者
都归约到同一个内部 artifact、同一个签名,所以验证与安装从不按源分支。本文是签名消息、
资产/清单形状、操作者配置与发布者签名流程的规范说明。

操作者点名要装哪个资产;该文件名在两个源里都是选择 key。无平台探测、无 arch 别名表。

---

## 1. 被签名的 artifact 消息

签名是对一个规范消息的 ed25519 —— UTF-8、`\n` 分隔、**无结尾换行**:

```
lode.artifact.v1
{name}
{version}
{sha256}
{run}
{exec}
```

| 字段 | 含义 | 来源 |
|---|---|---|
| `name` | **资产文件名**(如 `myapp-linux-x64.tar.gz`) | 选择 key;签名身份所绑定的对象 |
| `version` | 发布版本 | github:`tag_name` 去前导 `v`;native:`versions` 的 key |
| `sha256` | **原始下载文件**(解包前)的小写 hex | github:资产 `digest`;native:资产的 `sha256` |
| `run` | manifest 发布的裸跑启动命令覆盖(缺省为空字符串) | 资产的 `run` 字段,或 `""` |
| `exec` | manifest 发布的透传启动命令覆盖(缺省为空字符串) | 资产的 `exec` 字段,或 `""` |

`name` 是**资产文件名,不是应用名**。它是唯一绑定"这份签名授权的是*哪个* artifact"的
字段,因此防止把某个 artifact 的签名重放到别的资产或版本上。文件名还按惯例承载品牌与
平台,其后缀决定 format —— 这些都无需单独签名。

### 密钥

- ed25519,32 字节密钥以 base64 分发。`key_id` = `sha256(公钥)` 的前 16 个 hex 字符。
- 操作者在 `[trust].trusted_keys` 里以 `key_id:base64公钥` 钉住发布者。
- 签名:`sig = base64(ed25519_sign(私钥, message))`。
- 验证:当且仅当 `sig` 对**任一**受信密钥在重建消息上验过、**且**下载字节 hash 等于
  `sha256` 时,lode 才接受该 artifact。

### 签名绑定什么、不绑定什么

绑定:哪个资产(`name`)、哪个发布(`version`)、哪些字节(`sha256`)、哪些启动命令(`run`/`exec`)。**不**绑定
`platform`/`format`/`url` —— 它们由文件名推导或属操作者本地(见下)。因为 `name`
是文件名且被签,被篡改的目录无法把一份真签名挪到别的字节、别的资产、别的版本或注入恶意启动命令上。

## 2. 目录级(manifest)签名 —— 可选,present 才验

native 清单**可**携带顶层 `key_id` + `sig`。它是一层**可选**的防篡改证据,**永不强制**,
也**不**受 `require_signature` 门控:

- **存在** → 加载器验证它(在任何下载之前就抓出被换的 `latest`、增删的版本、被改写的
  资产摘要);存在但无效 → 拒绝。
- **缺失** → 任何 policy 下(含 `enforce`)都接受。

真正强制信任的因此**不是**目录签名,而是两层始终生效、与源无关的机制:**per-artifact**
签名(§1,由 `require_signature` 门控,绑定每个下载文件),以及**客户端禁降级 floor**
(§2a,保护 channel `latest` 指针不被回滚)。这正是为什么 GitHub release(无目录签名,§5)
与"只签资产的 native 目录"在 `enforce` 下都能正常工作。

规范消息(当目录**确实**被签时):

```
lode.manifest.v1
{name}
{key_id}
{canonical}        # channels + versions 的确定性、无 sig 序列化
```

`canonical` 按排序列出每个 `channel\t{name}\t{latest}`,以及每版每资产的
`asset\t{name}\t{sha256}`。GitHub 没有目录签名 —— 它的新鲜度来自 tag 权威(§5)。

### 2a. 客户端禁降级 floor(`latest` 回滚保护)

由于目录签名是可选的,channel `latest` 指针的防御放在客户端而非目录里:当**跟随**
`latest` 时(默认,或显式 `update --version latest`),加载器拒绝解析**低于 floor**的
版本 —— floor 是客户端已经承诺过的最高版本(`state.json` 里 `max(current, last_good)`)。
被篡改**或重放**的目录把 `latest` 指回更老的(哪怕是合法签名过的)版本,会在任何下载前被拒。

只有**跟随指针**的解析受此守卫。刻意降级始终允许:显式 `update --version X`、配置
`[update].pin`、或 `lode rollback`。比较按 semver 优先级;非 semver 的 `latest`/floor
无法排序,证明不了降级,故放行。

## 3. 资产命名与 format

- **文件名就是选择 key。** 操作者把 `[update].asset` 设为本机要的那个资产;lode 按
  `name` 在源的资产列表里匹配。
- **`format` 运行时从文件名后缀推导**(最长匹配):

  | 后缀 | format |
  |---|---|
  | `.tar.gz`、`.tgz` | `tar.gz` |
  | `.gz` | `gz` |
  | `.zip` | `zip` |
  | (其它 / 无) | `raw` |

  后缀具权威性 —— 命名资产时让后缀反映真实打包方式。

## 4. 启动命令覆盖(`run`/`exec`)与 format 推导

`entry` 概念已移除。**`format`** 运行时从资产文件名后缀推导(§3),不存储、不签名。manifest 资产可携带可选的 **`run`** 和 **`exec`** 字段,覆盖 operator 的 `[command].run`/`exec` 启动命令。这些字段被签进 per-artifact 签名消息(§1)和目录签名(§2)——在 `require_signature=auto`(有密钥)或 `enforce` 下,被篡改的目录无法注入恶意启动命令。

## 5. 源适配器 —— GitHub Releases

```toml
[update]
github = "owner/repo"
asset  = "myapp-linux-x64.tar.gz"
```

| 内部字段 | 来自 GitHub API |
|---|---|
| `name` | 资产 `name`(与 `asset` 匹配) |
| `version` | release `tag_name`(数字前的前导 `v` 去掉) |
| `sha256` | 资产 `digest`(去 `sha256:` 前缀),再对下载字节复验 |
| `sig` | 资产 **`label`**(API 唯一回传的任意字符串槽) |
| `url`(运行期) | `browser_download_url` |

- **版本指针 = tag 权威。** `channel = stable` → `/releases/latest`;其它 channel → 最新
  非草稿 prerelease;`pin` → `/releases/tags/{tag}`。
- `browser_download_url` 会 302 跳到 CDN 主机;这对验证透明 —— 验证用记录在案的字段,
  从不用跳转目标。

### 发布 —— GitHub Actions release workflow

push tag 触发 release 任务。**签名是可选的**:仅当配置了签名密钥(`LODE_SIGNING_KEY`
secret 非空)时才签,否则回退为上传未签名资产 —— 这样 fork 和没配密钥的仓库也能照常发版。
步骤:

1. **构建**各目标的资产到 `dist/`,按约定命名(`lode-<os>-<arch>.tar.gz`)。
2. 为该 tag **创建** release。
3. **逐个资产:有 key 才签,然后上传**:若 `LODE_SIGNING_KEY` 已设,签名并把签名作为资产
   `label` 上传(`file#label`);否则上传裸文件并告警「未签名」。

```yaml
# .github/workflows/release.yml
on:
  push:
    tags: ['v*']
permissions:
  contents: write                       # 创建 release + 上传资产
jobs:
  release:
    runs-on: ubuntu-latest
    env:
      GH_TOKEN: ${{ github.token }}
      LODE_SIGNING_KEY: ${{ secrets.LODE_SIGNING_KEY }}   # 可选 —— fork / 未配置时为空
    steps:
      - uses: actions/checkout@v4
      - name: Build release assets        # -> dist/lode-<os>-<arch>.tar.gz (+ lode-cli 二进制)
        run: ./scripts/build-release.sh "$GITHUB_REF_NAME"
      - name: Create release
        run: gh release create "$GITHUB_REF_NAME" --generate-notes --verify-tag
      - name: Sign (仅当配置了 key) and upload
        run: |
          set -euo pipefail
          TAG="$GITHUB_REF_NAME"
          for f in dist/lode-*.tar.gz; do
            if [ -n "${LODE_SIGNING_KEY:-}" ]; then
              sig=$(lode-cli sign "$f" --version "$TAG" --key-env LODE_SIGNING_KEY)
              gh release upload "$TAG" "$f#$sig"      # label = 签名
            else
              gh release upload "$TAG" "$f"           # 未签名
              echo "::warning::LODE_SIGNING_KEY 未设置 —— $(basename "$f") 以未签名上传"
            fi
          done
```

说明:

- **key 存在性判定。** secret 不能直接用在步骤的 `if:` 里,所以映射到 `env`,用
  `[ -n "${LODE_SIGNING_KEY:-}" ]` 判定。fork 和未配置的仓库里 secret 为空 → 任务上传未签名,
  绝不因缺 key 而失败。
- **`--key-env`** 从指定环境变量读取 base64 密钥种子,使私钥在 CI 里**不落盘**。key 必须放在
  受保护的仓库/组织 secret(或离线带外签名,获得最强托管)。
- **`lode-cli`** 是第 1 步构建出的 multi-call 二进制;用刚构建的它来签(其它项目需先安装
  `lode-cli`)。
- **未签名的后果。** 没有 `label` 的资产即未签名:消费端必须用 `require_signature = off`
  (或 `auto` 且无受信密钥 → 安装为 **UNVERIFIED** 并告警)。`require_signature = enforce` 下
  未签名资产会被拒绝。

## 6. 源适配器 —— native 清单

```toml
[update]
manifest = "https://releases.example.com/myapp/manifest.json"
asset    = "myapp-linux-x64.tar.gz"
```

清单是操作者自托管的 JSON,形状就是一份自托管的 release listing。schema `lode/v1`;每版
`assets[]` 按 `name`:

```json
{
  "schema": "lode/v1",
  "name": "myapp",
  "key_id": "<key_id>",
  "channels": { "stable": { "latest": "1.5.0" } },
  "versions": {
    "1.5.0": {
      "notes": "…",
      "assets": [
        { "name": "myapp-linux-x64.tar.gz",
          "url": "https://.../myapp-linux-x64.tar.gz",
          "sha256": "…", "sig": "…",
          "run": "./myapp", "exec": "./myapp", "size": 5242880 },
        { "name": "myapp-darwin-arm64.tar.gz",
          "url": "https://.../myapp-darwin-arm64.tar.gz",
          "sha256": "…", "sig": "…" }
      ]
    }
  },
  "sig": "<目录签名 —— 可选,见 §2>"
}
```

| 资产字段 | 必需 | 含义 |
|---|---|---|
| `name` | ✓ | 选择 key;与 `[update].asset` 匹配 |
| `url` | ✓ | 绝对下载 URL |
| `sha256` | ✓ | 原始文件的小写 hex |
| `sig` | enforce / auto+keys | 对 §1 消息(含 `run`/`exec`)的 base64 ed25519;内嵌,或在资产旁放 `.sig` sidecar |
| `run` | | 可选字面启动命令覆盖(已签名;覆盖 `[command].run`;见 §4) |
| `exec` | | 可选 CLI 透传命令覆盖(已签名;覆盖 `[command].exec`;见 §4) |
| `size` | | 期望字节数(额外完整性校验) |
| `auth` | | 默认 `true`;`false` = 不给该 URL 附 `[http].headers` |

- **版本指针。** `channels.<c>.latest` 的回滚由客户端禁降级 floor(§2a)在本地拦住 ——
  不需要目录签名来保护它。签目录(§2)仍推荐作为下载前的防篡改证据;`pin` 则彻底不再信任指针。
- native 可比 GitHub 多(`channels`、`notes`、detached `.sig`、`size`、
  `auth`);但全部仍在底层归约成 `(name, version, sha256) + sig`。

**发布:**

```bash
lode-cli manifest "$f" --version 1.5.0 --url "$URL" \
    --run ./myapp --exec ./myapp \
    --key private.key --into manifest.json     # 按 name upsert 资产,设 channels.latest;--run/--exec 可选
lode-cli manifest-sign --into manifest.json --key private.key   # 可选 §2 目录防篡改证据
```

把 `manifest.json` + 资产托管在任意 HTTPS URL。

## 7. 操作者配置(`lode.toml`)

```toml
[update]
github   = "owner/repo"           # 或  manifest = "https://.../manifest.json"(二选一)
asset    = "myapp-linux-x64.tar.gz"   # 本机要的资产文件名(选择 key)
channel  = "stable"
policy   = "auto"                 # off | check | auto
# pin    = "1.4.2"                # 锁定版本(禁用自动更新)

[trust]
require_signature = "enforce"     # off | auto | enforce —— 门控 PER-ARTIFACT 签名
                                  #   (§1)。off:仅完整性。auto:配了受信密钥后必需。
                                  #   enforce:始终必需。目录签名(§2)是 present 才验,
                                  #   不受此门控。
trusted_keys = ["<key_id>:<base64-公钥>"]
```

## 8. 组件职责(实施映射)

| 模块 | 职责 |
|---|---|
| `verify.rs` | §1 artifact 消息(`lode.artifact.v1`)与 §2 目录消息(`lode.manifest.v1`);`verify_artifact_sig` 对 `(name, version, sha256, run, exec)` |
| `manifest.rs` | 内部 `Manifest`,每版 `assets[]` 按 `name`;按 `name` 选资产;从后缀推 `format`;两个适配器(`fetch_github`、`fetch_native`)产出完全相同的内部模型 |
| `config.rs` | `[update].asset`;`manifest`/`github` 保持互斥 |
| `download.rs` | 按 `url` 拉取;`[http].headers` 仅同源附加;交叉校验 GitHub `digest` 并对下载文件重新 hash 比对签名里的 `sha256` |
| `authoring.rs` / `lode-cli` | `keygen`;`sign` → `(name, version, sha256)` 签名与 GitHub `label` 字符串;native `manifest` 组装 + `manifest-sign` 走 §2 目录形式 |

下游(`resolve_target`、install、supervise)共享、与源无关。

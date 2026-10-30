# Anki Desktop E2E 测试台

[Русский](README.md) · [English](README.en.md) · **简体中文**

<!-- i18n-source-git-blob: e27c55c68fc36811d7c07499e65daab60c0238d8 -->

`anki-e2e` 是 `anki-repo anki` 命令组（[契约](../anki-repo/desktop.md)）的常设本地
E2E 测试台。主机上的 Rust runner 构建包含**官方 Linux Anki Desktop** 的可复现
Docker 镜像，启动**一个隔离容器**（带 Xvfb 虚拟显示），并在其中通过**真实的**
`anki-repo`、Anki 原生安装程序、AnkiConnect 和 CrowdAnki 运行场景。Anki 的窗口、
确认框和弹出通知与功能同等重要，单独进行验证。

测试台**只能通过显式命令**启动。它不属于 `cargo test`、CI、build scripts 或 hooks；
runner 自身的单元测试不会启动 Docker。项目中没有 shell 脚本：编排、等待、点击、
截图、报告和清理均用 Rust 编写。

## 运行

```bash
cargo run -p anki-e2e -- full
```

场景选择：

| 命令 | 场景 | 证明的内容 |
|---|---|---|
| `full` | 以下全部 | 在干净测试台上的完整运行 |
| `smoke` | `smoke` | 发现 Anki、首次启动、`doctor` stopped/running、`launch`、无所有权时拒绝 `stop`、拒绝不安全的 endpoint |
| `addons` | `addons` | 无插件的干净 Anki → `setup --apply` → 原生安装程序 → 通过运行中的 Anki 安装第二个包 → 重复 setup 为 no-op |
| `patch` | `patch` | `patch install/status/repair/remove`、被擦除的 bootstrap、损坏的 payload、变更的 upstream、`restore`、重启后加载 |
| `crowdanki` | `crowdanki` | 带 PNG 的真实 export/import、CrowdAnki 对话框（OK 与 Cancel）、预校验拒绝、保留 GUID 迁移到干净库、`ready` |
| `process` | `process` | `launch/restart/stop`、模态窗口和 busy 时的拒绝、进程死亡、他人进程 |
| `errors` | `errors` | 安装程序中拒绝、禁用的插件、占用端口的他人监听者、他人 profile、损坏的插件 |
| `gui` | `addons`、`crowdanki` | 主要原生对话框和通知 |
| `run --scenarios a,b` | 列表 | 任意选择 |

常用参数（依次）：复现指定版本；失败后保留容器；通过 `vnc://127.0.0.1:5900`
观看屏幕；设置资源和总超时；仅选择版本并校验归档；删除测试台资源。

```bash
cargo run -p anki-e2e -- full --anki-version 26.09.3
cargo run -p anki-e2e -- addons --keep-container on-failure
cargo run -p anki-e2e -- gui --vnc --keep-container always
cargo run -p anki-e2e -- full --cpus 4 --memory 6g --timeout-minutes 120
cargo run -p anki-e2e -- resolve
cargo run -p anki-e2e -- clean --images --cache
```

`--require-clean` 在存在未提交更改时拒绝运行；没有该参数时，工作副本快照仍会记录在
报告中（`dirty`、路径列表和树的 SHA-256）。

`--timeout-minutes` 只限制容器内场景的执行，不包括归档下载和镜像构建：下载连续
60 秒未收到任何字节即失败，Ctrl+C 可及时中止卡住的网络读取和 `docker build`（先
SIGINT，再 SIGKILL）。部分下载的归档会被删除，不会进入缓存。`--cpus`、`--memory`
和 `--pids-limit` 限制的是场景容器，而不是 BuildKit 镜像构建。
只要数据持续到达，归档传输就没有总时限；等待 HTTP 响应头也受无进展检查和中断检查保护。

## Anki 版本

- **默认是最新稳定版。** 每次运行时 runner **完整**读取 `ankitects/anki` 的 GitHub
  Releases API（直到不满的一页），排除 draft、prerelease 和 beta，按数值比较版本，
  选择带有官方 `anki-<版本>-linux-<arch>.tar.zst` 制品的最高稳定版。GitHub 按发布
  时间排序，因此较晚发布、版本号较小的回移版本不影响选择。如果索引超出网络预算
  （5 页，每页 100 个），最大值不视为已确认：`ERROR`，不会从已读部分中选择。版本号
  不固定在 Dockerfile、代码或配置中。
- 如果最高稳定版没有 Linux 制品，或网络、索引不可用——`BLOCKED`/`ERROR`，
  不会回退到旧版本。
- `--anki-version X` 是显式回归运行；默认行为不变。prerelease 需要
  `--allow-prerelease`。
- 完整性：URL 必须与官方
  `https://github.com/ankitects/anki/releases/download/<tag>/<asset>` 一致；下载的
  SHA-256 与 GitHub 为该发布资产公布的 `digest` 比对。这是发布平台的校验和，
  不是维护者签名。如果该发布未公布校验和，运行被阻止；
  `--allow-unverified-archive` 允许运行，但报告会将校验和标记为自行计算。
- 缓存 `target/anki-e2e/cache/anki/<版本>/linux-<arch>/<sha256>/` 只在选定版本并
  重新校验 SHA-256 后使用；损坏的副本移到 `*.corrupt-*` 并重新下载。
- 离线模式只能显式启用，且只能使用预先校验过的制品：
  `--offline --anki-version X --sha256 <hex> [--anki-archive 路径]`。该模式不检查
  稳定版的新鲜度，报告会注明。

兼容性是 E2E 的**检验对象**，而不是选择旧版 Anki 的标准。如果最新稳定版不在
允许列表 `tools/anki-repo/src/desktop/compatibility.json` 中，需要插件的场景以
`BLOCKED` 结束，原因为 `anki_version_unsupported_by_desktop_adapter`，并给出实际
版本和阶段。

不得为了获得绿色结果而扩展允许列表：只有在核对 upstream 变更（涉及插件安装程序、
加载和重启的 Anki 代码；该版本 AnkiWeb 包的字节）并在其上完成完整 `full` 运行后，
才能加入新版本。在此之前，报告会如实显示需要已安装插件的场景为 `BLOCKED`。

## 镜像与容器

两个带标签 `io.github.anki-deck-toolkit.e2e=1` 的镜像：

- `anki-e2e/anki:<版本>-<arch>-<sha12>-<键>`，来自
  [`docker/anki.Dockerfile`](docker/anki.Dockerfile)：按 digest 固定的
  Ubuntu 24.04、官方构建的依赖、Xvfb、D-Bus 和 at-spi2-core、解压后的官方归档
  （再次校验 SHA-256）。**镜像中没有插件**：首次安装由场景验证。
- `anki-e2e/run:<树12>-<键>`，来自 [`docker/run.Dockerfile`](docker/run.Dockerfile)：
  `anki-repo` 和 `anki-e2e` 在 Docker 内（`rust:<MSRV>-bookworm`，MSRV 取自根
  `Cargo.toml`）根据 Git 跟踪或未忽略的工作副本文件快照构建。测试台的工作目录（`--work-dir`：
  缓存、快照、报告）无论位于何处都不会进入快照，即使位于仓库内且未写入
  `.gitignore`；包含已跟踪文件或包含仓库本身的目录会被拒绝。agent 在结果中报告
  树哈希，早期失败时也是如此：`PASS` 需要正向匹配（`provenance`），已证实的不匹配
  是独立错误 `provenance_mismatch`，而已知失败之后缺少证据不会取代其首要原因。共享 BuildKit Cargo target 缓存使用一个
  稳定目录，并受缓存锁保护。构建前删除旧方案遗留的 SHA-256 快照目录，然后运行
  `cargo clean -p`，仅清理 `anki-repo` 和 `anki-e2e` 的构建产物。Cargo 从当前快照重新构建
  这两个包，同时保留未变依赖的缓存产物。

容器：`--init`、用户 `10001`、`--cap-drop ALL`、`no-new-privileges`、CPU/内存/PID
限制、无发布端口的 bridge 网络（显式 `--vnc` 在 `127.0.0.1` 上除外），**无挂载**——
没有 home、Docker socket、工作副本、`decks/**`、`.asset-store/`。证据通过
`docker cp` 取出。报告记录实际的 `docker inspect` 以及对这些边界的单独检查。
Docker 客户端默认获得独立的空 `DOCKER_CONFIG`（`--docker-config inherit` 用于
非默认 context）。每个场景使用自己的空 `anki -b` 库；AnkiConnect 在容器**内部**的
loopback 上监听。

正常结束后容器会被删除，并加以验证。`--keep-container on-failure|always` 保留容器
用于诊断；`clean` 删除已结束的容器，并跳过 runner 仍在运行的容器。容器所有者由标签
`…e2e.host-runner`（machine-id、boot_id 和 runner 进程启动时间）确定，而不仅是 PID：
被重新分配的 PID 不算存活的 runner，之前内核启动遗留的容器视为过期，其他主机上
runner 的容器不会被触碰。没有该标签的旧方案容器，只有当其 PID 下运行的是 `anki-e2e`
时才视为活动。被中断运行遗留且没有 keep 标记的容器会在下次运行时删除。Ctrl+C 产生
`INTERRUPTED` 并执行清理。

`clean` 删除测试台的容器；`--images` 还会删除带测试台标签的镜像；`--cache` 删除工作
目录中的 `cache/`（Anki 归档）和 `build/`（源码快照），以及 Cargo 的 BuildKit cache
mount `anki-e2e-cargo-registry` 和 `anki-e2e-cargo-target`。cache mount 按类型
`exec.cachemount` 和精确 id 过滤，用 `docker builder prune` 定向删除，随后通过
`docker system df -v` 校验；守护进程共享的 BuildKit 层缓存不带测试台标签，不会被删除
（清理它需要在测试台之外有意执行 `docker builder prune`）。使用
`--docker-config inherit` 和非默认 buildx builder 时，校验只能看到守护进程自身的构建
缓存。任何要求删除却未删除的资源都会返回退出码 `3`。构建新的运行镜像后，runner 会删除
旧修订的运行镜像和源码快照；Anki 镜像和归档缓存保留到 `clean`。使用非默认 Docker
context 时，`clean` 也要传入 `--docker-config inherit`；若用户 context 处于活动状态，
isolated 模式会拒绝运行。并行命令会先串行使用共享缓存和 Docker 镜像，直到容器启动；
随后场景可以并行执行。

快照保留文件的 Unix 权限和符号链接本身，不读取链接目标。树哈希包含文件字节、权限和链接目标。

## 观察窗口和通知

Qt 在 AT-SPI 无障碍总线上发布控件（`QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1`）。agent
**被动**读取每个顶层窗口的角色、标题、可见文本、按钮、选中项和坐标，并从 X11
属性读取模态性和 `WM_TRANSIENT_FOR`。Anki 内部没有任何代码，因此在插件加载之前
Anki 显示的窗口（语言选择、`.ankiaddon` 安装确认）也可见。每个新出现或变化的窗口
都会立即对真实 Xvfb 显示截图，并写入 `events.jsonl`：步骤、时间、角色、标题、
文本、按钮、模态性、分类、测试动作和显示时长。

窗口按角色、标题、必需的文本片段和**精确的按钮集合**与目录 `src/agent/catalog.rs`
匹配。只有当窗口匹配某条规则且当前步骤预期该规则时，才会执行点击（XTEST，如同
用户操作）。未知窗口、出现在其他步骤中的已知窗口（例如 no-op setup 时的
"Installed"）或超出预期的重复，都会以 `FAIL`、证据和原因停止场景；这些窗口不会被
关闭。分类：`expected_informational`、`required_confirmation`、`warning_or_error`、
`duplicate_or_misleading`、`unknown_blocking`、`unexpected_notice`。截图本身不被
视为文本的证据：文本和模态性来自 AT-SPI 和 X11。

AT-SPI 或 X11 轮询失败时不会更新窗口快照：不完整的轮询不能证明窗口已关闭，之前的
快照也不会被当作新鲜快照。等待条件只在新鲜快照上判断，观察、命令和场景的结束都
需要一次成功的轮询。连续失败超过 10 秒为 `ERROR` `ui_observation_lost`；
`events.jsonl` 记录 `observation_degraded` 和 `observation_restored`。窗口每 150 毫秒
轮询一次，而基于 `anki-repo doctor` 和 `anki-repo job` 的等待每秒最多启动一次新的
CLI；启动次数记录在场景结果的 `commands_started` 中。

## 结果

`target/anki-e2e/runs/<id>/`（被 Git 忽略）：

- `report.json` 和 `report.md`——状态 `PASS`/`FAIL`/`BLOCKED`/`SKIPPED`/
  `ERROR`/`INTERRUPTED`、版本选择模式、检测到的稳定版、实际安装的版本、归档 URL
  和 SHA-256、镜像 ID、toolkit 的 commit 和树哈希、容器内 `anki-repo` 的 SHA-256、
  含检查和 UI 事件的场景、通知矩阵（"是否正常"、"是否需要点击"、"不操作会怎样"）、
  隔离和清理；
- `runner.log`、`build-*.log`；
- `agent/scenarios/<场景>/`——`result.json`、`events.jsonl`、`screenshots/`、
  `commands/`（argv、退出码、`anki-repo` 的 JSON 输出）、Anki 日志、测试牌组导出的
  副本。

必需证据包括两份报告、`agent-result.json`、每个场景的 `result.json` 和
`events.jsonl`、命令记录以及窗口和操作的截图。丢失其中任何一项都不能得到 `PASS`
（`evidence_missing`、`evidence_copy_failed`，报告未写入时为 `ERROR`），而已知的失败
原因仍是首要的 `stop`；之后的停止记录在 `secondary_stops` 中。

runner 退出码：`0` PASS、`1` FAIL、`2` BLOCKED/SKIPPED、`3` ERROR、
`130` INTERRUPTED。

## 要求

Linux 或 WSL2，带 Docker Engine（rootful 或 rootless）和 BuildKit，架构 x86_64 或
aarch64，可访问 GitHub、AnkiWeb、Docker Hub 和 Ubuntu 镜像源，默认每个容器
4 个 CPU 和 6 GB 内存。在测量环境中，测试台镜像、Cargo 缓存和归档占用约 6.6 GB
磁盘；Docker 基础镜像、构建临时数据和缓存增长余量还需要额外空间
（[测量详情](performance.zh-CN.md)）。首次运行构建镜像；之后的运行使用缓存。

## 性能

测试台各阶段的耗时、CPU、内存和磁盘占用及其测量方法记录在
[性能页面](performance.zh-CN.md)中；通用测量规则见
[方法说明](../../docs/performance/README.zh-CN.md)。

## 边界

测试台验证的是 **Linux** Anki，不能证明 `desktop.md` 中的 Windows/WSL 集成。它不
读写用户的 `Anki2`、`decks/**` 和 `.asset-store/`。可以在 `src/agent/scenarios.rs`
中添加新场景来扩展测试集，而无需改变 Docker 架构——例如用于未来的
`Anki → input → output → Anki` 交换。

# 测量契约：`benchmark-contract`

[Русский](../../ru/benchmark-contract/README.md) · [English](../../en/benchmark-contract/README.md) · **简体中文**

> **翻译提示：** 本译文可能未及时更新，技术内容以[俄语文档](../../ru/benchmark-contract/README.md)为准。欢迎提交翻译和校对改进。
`benchmark-contract` 是一个库和 CLI：加载基准场景目录 `benchmarks/`，校验声明和测量
结果，导入 Hyperfine 2.0 导出，并回答两个结果是否可比较。字段、配置、指标的含义以及
注册场景的规则见[目录指南](../performance/benchmarks.md)；通用测量规则见
[测量方法](../performance/README.md)。

该工具不运行也不测量任何东西：它读取目录 TOML、Cargo 清单和结果 JSON。它不写文件、
不访问网络，也不触碰卡组、媒体、Anki 配置文件、`decks/**` 和 `.asset-store/`。按目录
运行系列由 [`benchmark-runner`](../benchmark-runner/README.md) 完成，它使用本库。

## 命令

```bash
cargo run -p benchmark-contract -- validate
cargo run -p benchmark-contract -- list
cargo run -p benchmark-contract -- show anki-repo.validate
cargo run -p benchmark-contract -- preflight anki-e2e.full
cargo run -p benchmark-contract -- check-result result.json
cargo run -p benchmark-contract -- check-result --no-catalog result.json
cargo run -p benchmark-contract -- compare before.json after.json
cargo run -p benchmark-contract -- vocabulary
```

| 命令 | 作用 |
|---|---|
| `validate` | 加载目录并校验所有声明；输出诊断 |
| `list` | 场景：ID、修订、类型、状态、配置 |
| `show <id>` | 加载器所理解的类型化声明 |
| `preflight <id>` | 场景能否在此处测量：`platforms` 中的操作系统、`PATH`、cgroup v2、`perf_event_paranoid`；不检查网络和 Docker 守护进程 |
| `check-result <文件>…` | 校验结果；默认同时对照目录中的声明 |
| `compare <a> <b>` | 同一场景两个有效结果的可比性 |
| `vocabulary` | 指标、覆盖范围、来源、单位和运行限制的注册表 |

全局参数：`--catalog <目录>` —— 包含 `catalog.toml` 的目录（默认是当前目录最近的
父目录中的 `benchmarks/`）；`--json` —— 输出稳定的 JSON 而不是文本。文本诊断输出到
stderr。

## 退出码

| 代码 | 含义 |
|---|---|
| `0` | 目录或结果有效；`preflight` —— 平台受支持且所有已检查的依赖可用；`compare` —— 结果可比较 |
| `1` | 发现契约错误；`preflight` —— 平台不受支持，或依赖缺失或受限；`compare` —— 结果不可比较或无效 |
| `2` | 命令行参数错误 |
| `3` | 找不到目录、文件无法读取、找不到给定 ID 的场景（`show`、`preflight`），或需要有效目录的命令遇到无效目录 |

## 诊断

每条记录包含级别（`error` 或 `warning`）、稳定的机器代码、相对目录根的文件或文档名、
场景 ID 和字段路径（`profile[1].prepare[0].program.name`、`runs[2].values.<键>`）。
警告不会使目录无效。不受支持的 `schema_version` 是错误 `unsupported_schema_version`，
在解析其他字段之前报告。

## 库

| 模块 | 用途 |
|---|---|
| `catalog` | 目录的发现、加载和语义校验 |
| `scenario` | 声明类型以及带字段路径的结构检查 |
| `vocabulary` | 封闭词汇、指标注册表、运行限制 |
| `result` | 结果类型与校验、聚合值重新计算 |
| `hyperfine` | 解析并导入 Hyperfine 2.0 的 `--export-json` |
| `comparability` | 可比性规则 |
| `preflight` | 只读的平台与依赖可用性检查 |

## 检查

测试基于 `tests/fixtures/` 中的示例夹具构建自己的目录，也会检查真实的
`benchmarks/`；它们不需要 Docker、Anki 或 Hyperfine。它们属于
`cargo test --workspace --locked`。

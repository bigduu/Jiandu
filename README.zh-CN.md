# 简牍 Jiandu

[English](README.md) · [简体中文](README.zh-CN.md)

**给你所有的编程 agent 一份共享记忆。** Claude Code、Codex、Cursor 和 Claude Desktop
可以读写同一份本地记忆，在一个工具里记下的决定，换到另一个工具也能查到。不需要向量数据库，
也不调用 embedding API：直接对本地文件做确定性的 BM25 检索，支持中文分词。

[下载](https://github.com/bigduu/Jiandu/releases/latest) · [Bodhi](https://github.com/bigduu/Zenith) 组件之一 · [Bodhi 桌面版](https://github.com/bigduu/Bodhi-AI/releases/latest) · [MIT 开源](LICENSE)

- **三层记忆**：项目（同一项目里多个 agent 共享的决定）、会话（接着上次的活继续干）、全局
  （跨项目都有用的事实）。
- **一个 MCP 工具、19 种操作**：查询、读取、写入、合并、重建索引、Dream 概览快照等，都通过
  同一个 `memory` 工具完成。
- **数据就是普通文件**：存放在你指定的一个数据目录里；简牍从不调用模型或远程服务。
- **`main` 分支上、下一个版本才有的能力**：`jiandu ui`，一个只读的本地控制台，用来查看 agent
  记住了什么；以及逐次调用的宿主身份。目前最新的已发布版本是 **v0.2.0**，见[选择版本](#选择版本)。

<p align="center"><img src="docs/demos/memory-console.gif" alt="简牍在演示项目中搜索发布清单并打开保存的记忆。" width="720"></p>

[静态图片](docs/demos/memory-console.png) · [复现 MCP 写入和浏览器查询](docs/demos/reproduction.md)

这是在 Linux Chromium 中真实录制的 `main` 分支只读控制台（v0.2.0 中没有）。录制前通过真实的
stdio MCP 写入了一条明确标注的演示记忆。浏览器只是搜索并打开它，不写入记忆，也不运行模型。
没有使用任何个人记忆数据。

## 安装

已发布的 **v0.2.0**（两种方式都从源码编译，需要 Rust 1.95 或更新版本；Homebrew 会自动安装
Rust 作为构建依赖）：

```sh
# Homebrew（编译 v0.2.0 源码 tag）
brew tap bigduu/tap
brew install bigduu/tap/jiandu

# 或者用 Cargo
cargo install jiandu-mcp --version 0.2.0 --locked
```

可执行文件名是 `jiandu`，可以用 `jiandu --help` 检查。安装不会启动任何服务，也不会配置任何 MCP
客户端。用 `which jiandu` 查看绝对路径（Homebrew 在 Apple Silicon 上通常是
`/opt/homebrew/bin/jiandu`，Intel Mac 上通常是 `/usr/local/bin/jiandu`）。

## 接入你的 agent

要共享项目记忆，每个客户端都要用**同一个 `--data-dir`** 和**同一个 `--project-id`** 启动。
每个客户端各用一个自己的 `--session-id`：v0.2.0 必须提供，`main` 上它是可选的默认值。ID 只能
包含字母、数字、`-` 和 `_`。把 `/Users/you` 换成你的用户目录；JSON 和 TOML 不会展开 `~`。

**Claude Code：**

```sh
claude mcp add --scope user jiandu -- /opt/homebrew/bin/jiandu \
  --data-dir "$HOME/.jiandu" --project-id my-app --session-id claude-code
```

**Codex**（`~/.codex/config.toml`）：

```toml
[mcp_servers.jiandu]
command = "/opt/homebrew/bin/jiandu"
args = ["--data-dir", "/Users/you/.jiandu", "--project-id", "my-app", "--session-id", "codex"]
```

**Cursor**（`~/.cursor/mcp.json`）/ **Claude Desktop**（`claude_desktop_config.json`）：

```json
{
  "mcpServers": {
    "jiandu": {
      "command": "/opt/homebrew/bin/jiandu",
      "args": ["--data-dir", "/Users/you/.jiandu", "--project-id", "my-app", "--session-id", "cursor"]
    }
  }
}
```

对于通用 MCP 客户端，一个进程只服务一个项目。不加 `--project-id` 时，通用 MCP 客户端只能使用
全局记忆：项目相关操作会直接报错，而不是去猜项目。有多个项目时，每个项目加一条服务配置，或者给
每个项目单独写 `.cursor/mcp.json` / 用 `claude mcp add --scope project` 添加。

试试：在一个 agent 里说*“在项目记忆里记下：我们只在周五发版。”*，再到连接同一项目的另一个
agent 里问*“我们什么时候发版？”*。全新的数据目录第一次查询会提示缺少词法索引，agent 应对该
范围执行一次 `rebuild` 后再查询。

## 选择版本

| 途径 | 可用的功能 |
| --- | --- |
| [已发布的 v0.2.0](https://github.com/bigduu/Jiandu/releases/tag/v0.2.0) | 基于 stdio MCP 的共享记忆、每个进程固定的项目/会话默认值、Dream 快照，以及一次性的 Bamboo 导入。 |
| `main`（下一个版本，需从源码构建） | 另外包含只读的浏览器控制台，以及下文介绍的逐次调用宿主身份元数据。这些新增功能不在 v0.2.0 中。 |

源码版本号已经改成 `0.3.0`，为下一个版本做准备，但还没有发布。想用已发布的接口约定就安装
v0.2.0；想用控制台和逐次调用上下文就构建当前源码。两者都需要 Rust 1.95 或更新版本来构建。
[审计记录](docs/readme-audit.md)。

简牍只认一个权威数据根目录，通常是 `~/.jiandu`。切换之后，Bamboo 原生记忆和所有 MCP 客户端都
必须使用这个由简牍管理的同一个根目录；`~/.bamboo` 不能继续作为第二个权威的持久记忆存储。

## 组件

- `jiandu-memory` 负责持久化、维护以及词法 / BM25 / 中文检索。
- `jiandu-mcp` 通过 stdio 把存储暴露为一个名为 `memory` 的 MCP 工具，用 `action` 参数选择
  19 种记忆操作之一。同一个二进制还提供本地只读的浏览器控制台。

## 记忆范围

- **会话（Session）**：某个由宿主标识的 agent 工作流的临时上下文。
- **项目（Project）**：同一项目里多个 agent 共享的持久知识。MCP 宿主用一个稳定、不透明的
  `project_id` 授权每次调用。
- **全局（Global）**：真正跨项目都有用的持久知识。

## 从源码构建与通用配置（`main`）

要使用 `main` 上的控制台和逐次调用上下文，请构建当前源码：

```shell
cargo build --release --locked -p jiandu-mcp --bin jiandu
```

使用生成的 `target/release/jiandu` 的绝对路径。`main` 构建只需要 `--data-dir`；`--project-id`
和 `--session-id` 是给无法发送简牍上下文元数据的客户端准备的可选默认值（见
[宿主集成](#宿主集成逐次调用身份main)）。下面这个最小配置足以使用全局记忆：

```json
{
  "mcpServers": {
    "jiandu": {
      "command": "/absolute/path/to/Jiandu/target/release/jiandu",
      "args": [
        "--data-dir", "/absolute/path/to/.jiandu"
      ]
    }
  }
}
```

v0.2.0 二进制不接受这种最小写法，因为它要求 `--session-id`；v0.2.0 请使用
[接入你的 agent](#接入你的-agent) 里的配置。

宿主可能会把工具命名为 `mcp__jiandu__memory`。第一次试用时，请用一个新的专用数据目录和全局记忆，
它不需要任何身份上下文。让宿主先查询，写入一条确认过的、不敏感的跨项目事实，再从连接同一根目录
的另一个连接里查询它。新的根目录没有词法索引：如果第一次查询报告 `lexical index is missing`，
对同一范围调用 `rebuild`，然后在写入前重新查询。只在出现这条诊断时才重建。下面是几次独立的
`memory` 工具调用：

```json
{"action":"query","scope":"global","query":"fictional user language preference"}
```

如果第一次调用报告缺少索引，运行：

```json
{"action":"rebuild","scope":"global"}
{"action":"query","scope":"global","query":"fictional user language preference"}
```

查询成功后：

```json
{"action":"write","scope":"global","type":"user","title":"Fictional user language preference","content":"This fictional demo user prefers replies in English across projects."}
{"action":"query","scope":"global","query":"fictional user language preference"}
```

这些调用使用的是刻意虚构的演示数据。在配置为提供简牍项目/会话上下文的宿主中，普通工具调用同样
不需要填写身份字段：

```json
{"action":"query","scope":"project","query":"release decision"}
{"action":"session_read"}
```

缺少项目上下文时项目操作会失败；缺少会话上下文时 `session_*` 操作会失败。它们不会编造身份，也
不会悄悄退回到全局记忆。请让宿主集成方补齐缺失的上下文；不要为了绕过这个边界把项目专属的事实
移到全局记忆里。通用 MCP 宿主必须显式实现简牍的元数据扩展，这些依赖上下文的调用才能在没有专用
进程默认值的情况下工作。

### 宿主集成：逐次调用身份（`main`）

连接时只需要 `--data-dir`。全局记忆不需要会话或项目身份。一个连接可以服务多个项目和工作流：
宿主在每个 `tools/call` 请求中单独提供身份，放在模型生成的工具参数之外：

```json
{
  "name": "memory",
  "arguments": {"action":"query","scope":"project","query":"release decision"},
  "_meta": {
    "io.github.bigduu.jiandu/context": {
      "project_id": "project-1",
      "session_id": "agent-session-1"
    }
  }
}
```

这是 `tools/call` 请求的 `params` 对象。这个元数据键是简牍的扩展，宿主必须显式支持并填写；MCP
本身不会自动提供当前的项目或会话身份。项目操作需要宿主授权的 `project_id`；`session_*` 操作
需要宿主提供的 `session_id`。操作不需要时可以省略对应字段。工具参数里的 `project_key` 只能声明
当前宿主的项目，不能授予访问权限。每个独立的工作流请使用不同的会话 ID。

如果客户端无法提供这些元数据，宿主集成方可以配置专用进程的默认值。对于只服务一个工作流的进程，
`--session-id <ID>` 和 `--project-id <ID>` 仍然是可选的启动默认值。逐次调用的上下文会完全替换
这两个默认值；`{}` 表示在这次调用中显式清空它们。上下文从不在调用之间保留，格式错误的上下文会被
拒绝，而不是退回默认值。不支持这种元数据的客户端可以使用全局记忆或这些固定默认值。

Rust 宿主可以构造 `MemoryExecutionContext::default()`，并使用
`MemoryServer::execute_with_context` 为每次调用提供可信上下文。解析规则、权限和兼容性细节见
[逐次调用身份设计](docs/design/mcp-call-context.md)。共享项目记忆的 agent 使用同一个数据目录和
同一个项目身份。先查询再写入，持久条目保持简洁，永远不要直接编辑简牍的数据文件。

## 本地控制台（`main`）

```shell
jiandu ui
# 或者指定 agent 使用的同一个权威根目录：
jiandu ui --data-dir /absolute/path/to/.jiandu --port 9123
```

打开命令打印出的 `http://127.0.0.1:<port>/` 地址。默认根目录是 `~/.jiandu`，端口 `0` 表示让系统
分配一个可用端口。不需要会话或项目身份。按 Ctrl-C 停止；这个命令只在 IPv4 回环地址上提供服务。

可以浏览全局知识、切换正式登记的项目，或选择已保存的会话。可以搜索和翻页浏览记忆、按状态筛选，
并查看完整内容、标签、时间戳和来源。持久范围还会显示词法索引是否可用，以及 Dream 快照的新鲜度
和内容。会话显示临时笔记，不会被推断归属到某个项目。

控制台使用现有的 MemoryStore 和内置资源，不需要单独的前端构建或 agent 运行时。它不会写入记录、
访问信号、索引、会话状态或 Dream 快照。索引缺失或无效时仍然可以用空查询浏览；需要时请通过已授权
的 memory 工具重建。这个版本没有编辑、重建或生成 Dream 的控件，只列出正式登记的项目，不列出旧的
目录。见[控制台设计](docs/design/local-console.md)。

## 可选的 agent Skill

标准的 [`jiandu-memory` Skill](skills/jiandu-memory/SKILL.md) 教宿主模型以最低成本完成
查询/读取/写入、Dream 和确定性维护流程。它是可选的：没有这个 Skill 时，简牍实时的 MCP 描述、
schema 和结构化响应仍然是正确性的约定。

把标准的 `skills/jiandu-memory/` 目录复制或软链接到宿主的一个原生位置：

| 宿主 | 个人 | 仓库 |
| --- | --- | --- |
| Codex | `$HOME/.agents/skills/jiandu-memory/` | `$REPO_ROOT/.agents/skills/jiandu-memory/` |
| Claude Code | `~/.claude/skills/jiandu-memory/` | `$REPO_ROOT/.claude/skills/jiandu-memory/` |

宿主按自己的权限模型发现并启用 Skill；简牍不会安装或启用它。见官方的
[Codex Skill 位置](https://developers.openai.com/codex/skills#where-codex-loads-local-skills)
和 [Claude Code Skill 位置](https://code.claude.com/docs/en/slash-commands#where-skills-live)。

## Dream 概览

Dream 是宿主为全局记忆和每个已授权项目生成的一份简短概览快照。它是派生出的文字，不是权威事实，
从不计入主题数量、生命周期操作或词法检索。

先调用 `dream_read`。它返回缺失的冷启动状态或当前快照，以及权威记忆的 `current_generation` 和
一个参考性的 `stale` 标记。拥有模型、提示词、节奏和预算的宿主可以生成最多 12,000 个字符的
Markdown，然后用开始生成前观察到的代际调用 `dream_publish`：

```json
{
  "action": "dream_publish",
  "scope": "project",
  "source_generation": "<current_generation from dream_read>",
  "content": "## Current orientation\n\n- ..."
}
```

简牍会原子地发布正文和元数据；如果期间权威记忆发生了变化，就拒绝这次结果。升级已有存储后如果
缺少代际标记，需要对该授权范围执行一次 `rebuild`。涉及事实判断时，请始终使用 `query`/`get` 和
当前工具；Dream 只是一个低成本的初步概览。简牍从不选择或调用模型/provider，也不安排 Dream 的
生成时间。

## 从 Bamboo 一次性导入

在 Bamboo 把原生记忆存储切换到简牍根目录之前，先停止 Bamboo 的记忆写入，或做一个静态快照。目标
目录必须不存在或为空，并且不能位于 Bamboo 源目录内：

```shell
jiandu import-bamboo \
  --source-data-dir /absolute/path/to/.bamboo \
  --data-dir /absolute/path/to/.jiandu
```

这个命令只读取 Bamboo 源数据，不做修改。它只导入 `memory/v1/scopes/global/topics/*.md` 和
`projects/<ProjectId>/memory/v1/topics/*.md` 下当前的权威持久主题；会话笔记、Dream/Ledger/
计划数据、索引、视图、日志、锁、状态和迁移管理数据都不会复制。每个主题在构建同级暂存存储之前都会
校验，然后按简牍当前的类型化 schema 渲染一次，因此 `embedding_ready` 这类已废弃的元数据不会变成
新的简牍状态。每个导入的范围重建一次；只有当原始源数据和当前 schema 暂存数据的身份都与校验预期
一致时，才会发布完成的存储。JSON 结果会报告扫描/导入/失败的数量，以及原始源主题路径/字节和导入后
当前 schema 路径/字节各自的 SHA-256 身份。这个命令从不删除 Bamboo 源数据，也拒绝覆盖已初始化的
简牍根目录。

切换成功后，Bamboo 应使用这个由简牍管理的根目录构造其原生 `jiandu-memory::MemoryStore`。其他
agent 应通过 stdio 用同一个 `--data-dir` 启动 `jiandu`；不要双写，也不要继续把 Bamboo 源目录
作为备用记忆根目录。

## 宿主集成

Bamboo 可以直接使用 `jiandu-memory`，并在组装动态上下文时优化召回。排序、提示词中的位置和 token
预算仍由 Bamboo 负责。Bamboo 还负责 Dream 的生成提示词、模型选择、节奏、失败策略和提示词位置；
简牍只保存最终带代际标记的快照。其他 agent 把 `jiandu-mcp` 当作共享记忆使用，不依赖 Bamboo 的
运行时类型。

## 验证

```shell
cargo fmt --all -- --check
cargo metadata --locked --all-features --format-version 1
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
```

简牍采用 [MIT 许可证](LICENSE)。

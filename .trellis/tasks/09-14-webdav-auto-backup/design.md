# 技术设计：WebDAV 配置同步

## 1. 分层

参考 LiveAgent 的分层（`docs/features/config-backup-sync.md`），按 codeg 的目录约定落位：

| 层 | 路径（新建） | 职责 |
|---|---|---|
| 快照采集 / 校验 / 应用 | `src-tauri/src/commands/config_sync/snapshot.rs` | `collect_snapshot_core` / `validate_manifest` / `apply_snapshot_core`；应用前自动落一份本地回退快照 |
| 域定义表 | `src-tauri/src/commands/config_sync/domains.rs` | 单一真相表：每个域的 id、采集器、应用器、字段黑名单 |
| 可移植偏好白名单 | `src-tauri/src/commands/config_sync/portable_keys.rs` | `app_metadata` 允许清单常量 |
| 本地导入导出 | `src-tauri/src/commands/config_sync/local_io.rs` | 文件对话框 + 解析校验 + 写入 |
| WebDAV 编排 | `src-tauri/src/commands/config_sync/webdav_sync.rs` | 同步配置存取、远端路径拼装、上传/下载、sha256 校验 |
| WebDAV 传输 | `src-tauri/src/network/webdav.rs` | PROPFIND / MKCOL / PUT / GET；超时分级、响应体上限、日志脱敏、网盘定向文案 |
| 自动同步 | `src-tauri/src/commands/config_sync/auto_sync.rs` | 周期任务、哈希比对、抑制守卫 |
| 命令入口 | `src-tauri/src/commands/config_sync/mod.rs` | `#[tauri::command]` 薄封装 |
| 前端 API | `src/lib/config-sync.ts` | 命令封装 + 事件类型 |
| 设置 UI | `src/components/settings/config-sync-settings.tsx` | 本地备份组 + WebDAV 同步组 |

传输层放 `network/` 而非 `commands/`：它是通用 HTTP 能力，`network/proxy.rs` 已经在那儿，将来若有其他远端后端也在同一层。

## 2. 快照格式

```jsonc
// config.json
{
  "schemaVersion": 1,
  "domains": {
    "modelProviders": [ /* 行数组 */ ],
    "agentSettings":  [ ... ],
    "customAgents":   [ ... ],
    "quickMessages":  [ ... ],
    "taskTemplates":  [ ... ],
    "preferences":    { "<key>": "<value>" }
  }
}
```

```jsonc
// manifest.json
{
  "schemaVersion": 1,
  "encryption": "none",          // v1 恒为 none，为后续加密预留
  "createdAt": "2026-09-14T...Z",
  "appVersion": "0.30.7",
  "sourceDevice": "<hostname>",  // 只用于确认对话框展示
  "config": { "size": 12345, "sha256": "..." },
  "counts": { "modelProviders": 3, "...": 0 }
}
```

- **schemaVersion 单调递增**。读到 `> 当前` 直接拒绝并提示升级；读到 `< 当前` 正常读，缺失的域按空处理（`#[serde(default)]`）。
- `sourceDevice` 只用于 UI 展示，绝不参与任何逻辑判断。

## 3. 域定义表（单一真相）

codeg 的备份引擎已经用「一张表两个消费者 + 一致性测试」的模式解决过采集/应用漂移问题（见 `commands/backup/sections.rs` 顶部注释：两份手工维护的列表漂移导致 `acp-transcripts` 丢失）。配置同步照抄这个模式：

```rust
pub struct ConfigDomain {
    pub id: &'static str,
    pub collect: fn(&DatabaseConnection) -> BoxFuture<Result<Value, AppCommandError>>,
    pub apply:   fn(&DatabaseTransaction, &Value) -> BoxFuture<Result<usize, AppCommandError>>,
}
pub const CONFIG_DOMAINS: &[ConfigDomain] = &[ ... ];
```

配套测试 `every_domain_is_collected_and_applied`：加了域但没接上应用器 → 测试失败，而不是静默丢数据。

### 各域的字段黑名单

**采集时剔除**，不要指望应用时再过滤——快照一旦上传，明文就落在网盘上了。

| 域 | 剔除字段 | 原因 |
|---|---|---|
| `agentSettings` | `installed_version` | 本机实际装的 CLI 版本，跨机无意义且会误导版本检查 |
| `customAgents` | `skills_dir` | 绝对路径 |
| 全部 | `created_at` / `updated_at` / `id` | 时间戳与自增主键不跨机传播，见 §4 |

### 明确排除的表

`work_task_settings` 虽然名字像配置，但它有 `folder_id` 外键，而 `folder` 是设备本地的绝对路径记录。跨机同步会指向错误的项目甚至不存在的 id。只同步 `work_task_template`。

其余排除项见 prd.md R1。

## 4. 应用语义：按自然键 upsert，不删本地多余行

**不采用「清空整表再插入」。** 理由：

- `agent_setting.model_provider_id` 是指向 `model_provider.id` 的外键；`conversation` 等表也可能引用智能体标识。清表重插会重排自增主键，打断这些引用。
- 清表意味着「A 机器上没有的东西会在 B 机器上被删掉」。用户从 A 拉一份配置到 B，预期是「把 A 的配置带过来」，不是「让 B 变成 A 的克隆并丢掉 B 独有的服务商」。

因此：**按自然键 upsert，命中则更新，未命中则插入，本地多余的行原样保留。**

| 域 | 自然键 |
|---|---|
| `modelProviders` | `(agent_type, name)` —— 该表无唯一索引，由实现层保证组合唯一性判断 |
| `agentSettings` | 表上既有的唯一键（`m20260226_000001_agent_setting.rs` 中定义） |
| `customAgents` | `registry_id`（表上唯一） |
| `quickMessages` | `title` |
| `taskTemplates` | `name` |
| `preferences` | `app_metadata.key`（白名单内叠加，白名单外本机原值保留） |

- `agent_setting.model_provider_id` 的重映射：`modelProviders` 域先于 `agentSettings` 应用；快照里 `agentSettings` 记录的不是数字 id 而是所属服务商的自然键，应用时查出本地 id 再写入。这是 v1 必须处理的一处真实映射，不能偷懒直接搬 id。
- 整个应用过程在**一个事务**里；任一域失败则全部回滚。
- 「不删本地多余行」是有意的取舍，需要在 UI 文案里讲清：同步是合并，不是镜像。

## 5. WebDAV 传输层

```rust
pub struct WebdavClient { http: reqwest::Client, base: Url, auth: (String, String) }
impl WebdavClient {
    pub async fn probe(&self) -> Result<(), WebdavError>;              // PROPFIND Depth:0
    pub async fn ensure_dir(&self, rel: &str) -> Result<(), WebdavError>; // 逐级 MKCOL
    pub async fn put(&self, rel: &str, body: Vec<u8>) -> Result<(), WebdavError>;
    pub async fn get(&self, rel: &str) -> Result<Option<Vec<u8>>, WebdavError>; // 404 → None
}
```

- `PROPFIND` / `MKCOL` 用 `reqwest::Method::from_bytes(b"PROPFIND")` 构造，**不解析响应 XML**。探活只看状态码，`ensure_dir` 只看 `201/405`（405 = 已存在）。因此**不需要新增 XML 解析依赖**。
- 分级超时：探活 15s，PUT/GET 120s（快照几十 KB，超时给足只为容忍慢网盘）。
- `get` 设响应体上限（如 8 MiB），超限直接报错。防止异常/恶意远端撑爆内存。
- **日志脱敏**：`WebdavError` 的 `Display` 与所有 `tracing` 调用只允许出现 `{method} {rel_path} -> {status}`。禁止打印 base URL（可能带用户名）、Authorization 头、密码。需要一条单测断言错误信息里不含密码。
- 定向错误文案：401（坚果云等需应用密码而非登录密码）、403、404（远端目录不存在）、409（父目录缺失）、507（配额满）各给可区分的 i18n key，其余归为通用错误。

### 远端路径

`{remoteDir}/v{PROTOCOL_VERSION}/{profile}/{manifest.json|config.json}`，默认 `remoteDir = "codeg"`、`profile = "default"`。

`remoteDir` 与 `profile` 需做路径分段校验：非空、不含 `/`、`\`、`..`，避免拼出越界路径。

## 6. 上传顺序与串行化

- **先 PUT `config.json`，再 PUT `manifest.json`。** manifest 是「这份备份可用」的信号，最后写。中途失败时远端是旧 manifest + 新 config，下载侧的 sha256 校验会拦下，而不会把残缺配置当合法快照应用。
- 上传前 `ensure_dir` 逐级建目录。
- 所有远端读写走一个进程内 `tokio::sync::Mutex`。上传是两步 PUT，并发会让两个文件来自不同快照。
- 下载：先 GET manifest，再 GET config，算 sha256 与 manifest 比对，不一致直接拒绝且不触碰本地数据。

## 7. 自动同步

### 触发：周期性哈希比对

```
每 intervalMinutes（默认 5，最小 1）：
  若被抑制 → 跳过
  采集快照 → 序列化 → sha256
  若 == last_uploaded_sha256 → 跳过（零网络请求）
  否则上传；成功后记录 sha256 与 lastSyncAt；失败记录 lastError
```

**为什么不照抄 LiveAgent 的「写入咽喉标脏 + 防抖」**：LiveAgent 有 6 个集中的 `save_*` 作为各域在 SQLite 侧唯一的写入咽喉，插桩点少且不会漏。codeg 的配置写入分散在 `model_provider.rs` / `custom_agents.rs` / `quick_messages.rs` / `work_task.rs` / 各 settings 命令等几十处，逐处插桩既是大面积改动又必然随新功能漏掉。快照只有几十 KB，一次采集 + 哈希是毫秒级的本地开销，周期比对天然不会漏，且「无变化就零请求」的效果与防抖等价。

- 启动后延迟 60s 做首次比对，避开启动风暴。
- `last_uploaded_sha256` 持久化（`app_metadata`），进程重启后不会因为「内存里没有基线」而白传一次。
- 失败退避：连续失败时把下次尝试推迟到 `interval * 2^min(n,4)`，成功后归零。不禁用配置，不弹通知。

### 抑制

`AutoSyncSuppressionGuard`（RAII 引用计数），下载并应用远端快照期间持有。应用快照会改本地配置，不抑制就会把刚拉下来的数据原样推回去。

**本地文件导入路径有意不抑制**——用户主动从文件导入的配置应当传播到远端。

### 状态反馈

后台自动同步结果走 Tauri 事件 `config-sync://status`，载荷 `{ lastSyncAt, lastError }`。手动同步的成败由命令返回值同步告知前端，不走这个事件——**收到事件即意味着「后台自动同步」**。

## 8. 同步配置与凭据

独立键 `config_sync_settings`（`app_metadata`），**不在 `portable_keys` 白名单内**。若随快照流转，A 的凭据会覆盖 B，形成同步循环。

需要一条单测：`portable_keys` 白名单与 `config_sync_settings` / `system_proxy_settings` / `web_service_*` / `github_accounts` 的交集为空。

```rust
struct ConfigSyncSettings {   // 存储态
    enabled: bool, server_url: String, username: String, password: String,
    remote_dir: String, profile: String,
    auto_sync: bool, interval_minutes: u32,
}
struct ConfigSyncSettingsView { // 回传前端，无 password
    ..., has_password: bool,
}
```

**密码回填**：保存时前端传 `password_touched: bool`，为 `false` 则沿用库里旧值。LiveAgent 记录的真实 bug：UI 给密码框填掩码占位符后原样提交，把占位符当新密码写库，下次同步认证失败。需要一条单测覆盖。

## 9. 回退快照

应用远端/导入快照前，先把当前配置采集一份写到 `~/.codeg/config-snapshots/<rfc3339>.json`，保留最近 10 份（超出按时间删最旧）。目录需加入 `backup/sections.rs` 的排除考量确认——它不在任何 `MANAGED_SECTIONS` 的 live path 下，无需改动，但要在实现时确认这一点。

## 10. 双模式与 cfg

- 采集/应用/传输/编排全部写成运行时无关的 `_core` 形式（只接 `&DatabaseConnection` / `&EventEmitter`），遵循 codeg 双模式约定。
- 仅 `#[tauri::command]` 薄封装与自动同步任务的启动挂在 `tauri-runtime` 下。
- v1 不提供 Web handler，但 `_core` 的形态保证以后能加而不重构。
- 验收必须跑 `cargo check --no-default-features --bin codeg-server`。

## 11. 测试策略

Rust：
1. 域表一致性：`every_domain_is_collected_and_applied`
2. 采集剔除黑名单字段：快照 JSON 中不含 `installed_version` / `skills_dir` / `id` / 时间戳
3. 快照不含凭据：整份 JSON 字符串搜不到 WebDAV 地址、账号、密码
4. 白名单叠加：应用后白名单外的本机 `app_metadata` 键原值保留
5. round-trip：采集 → 应用到空库 → 再采集，两份快照等价
6. upsert 语义：本地多余行在应用后仍存在；同自然键的行被更新而非重复插入
7. `agent_setting.model_provider_id` 跨机重映射正确
8. manifest 校验：sha256 不匹配 → 拒绝且本地未改动；`schemaVersion` 更高 → 拒绝并给升级提示
9. 密码回填：`password_touched: false` 不覆盖旧密码
10. 白名单与凭据键交集为空
11. 传输层错误脱敏：错误信息不含密码
12. 哈希比对：无变化时不产生上传调用（用假传输层断言调用次数为 0）
13. 抑制守卫：持有期间周期任务跳过

前端 vitest：
14. 同步配置表单：未改密码时提交 `passwordTouched: false`
15. 状态事件更新 UI 的「上次同步 / 上次错误」显示

WebDAV 传输层的真实网络行为不做自动化，在 prd.md 验收清单里对真实网盘手工走。

## 12. 依赖

不新增 crate。`reqwest`（已在）+ `sha2`（已在）+ `serde_json`（已在）+ `hostname` 用 `whoami`/环境变量替代或直接省略即可——若 `sourceDevice` 取不到就填 `"unknown"`，不为它引依赖。

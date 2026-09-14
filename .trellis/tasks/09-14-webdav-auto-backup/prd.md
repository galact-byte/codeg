# WebDAV 配置同步

父任务：`.trellis/tasks/09-14-issue-633-close-behavior-webdav`
来源：issue #633 诉求 2

## Goal

把 codeg 自己拥有的配置数据打包成一份小体积 JSON 快照，支持导出到本地文件，或同步到用户自己的 WebDAV 网盘，以便换机 / 多设备复用配置，并支持后台自动上传。

## 范围决策（已与需求方确认）

issue #633 原文第二张截图就是一个**配置同步**面板（WebDAV 同步 / 本地备份 / 备份范围三张卡，范围卡明写「不包含：对话历史、记忆库、上传文件、SSH 私钥」）。作者要的正是配置同步，不是全量数据备份——本任务的范围与作者诉求一致。

参考 LiveAgent 的 `docs/features/config-backup-sync.md`，采用**配置快照**模型而非全量归档上传：

- codeg 现有备份是全量加密 zip（数据库 + uploads + 外部 CLI 会话记录），体积可达 GB 级。定期整包推送到网盘会触发限流、吃满配额，不可行。
- 配置快照体积在几十 KB ~ 几百 KB 量级，适合高频自动上传。
- **不加密。** 安全边界是「WebDAV 端点由用户自己持有、自己认证」。这与 LiveAgent 的取舍一致。manifest 预留 `encryption` 字段（v1 恒为 `"none"`），为后续加密留无破坏性升级路径。
- 全量归档备份维持现状：仅手动导出，不进入本任务范围。

## Requirements

### R1 快照范围（schema v1）

**在范围内**（全部存于 SQLite，采集与应用完全在后端完成，前端不拼装快照内容）：

| 域 | 来源 | 说明 |
|---|---|---|
| `modelProviders` | `model_provider` 表 | 含 API 密钥（明文） |
| `agentSettings` | `agent_setting` 表 | 排除 `installed_version`（本机实际安装的 CLI 版本，设备本地态） |
| `customAgents` | `custom_agent` 表 | 排除 `skills_dir`（绝对路径） |
| `quickMessages` | `quick_message` 表 | |
| `taskTemplates` | `work_task_template` | 不含 `work_task_settings`：它以 `folder_id` 外键挂在设备本地的项目记录上，跨机会指向错误甚至不存在的项目 |
| `preferences` | `app_metadata` 白名单键 | 见 R2 |

**明确不在范围内**：

- 对话历史（`conversation` / `message` / `acp-transcripts`）、`token_usage_*`、`work_task` 运行记录、`automation_run`、`canvas_node`、`opened_tab`
- `folder` / `folder_command` / `folder_group` / `folder_link`（工作区项目全是绝对路径，跨机必然失效）
- `work_task_settings`（按 `folder_id` 挂在上述本地项目记录上，同理失效）
- `uploads`、`backgrounds`、`pets` 图片资源、`skills` 目录（本体是磁盘目录，只同步引用等于同步一份指向空气的清单——LiveAgent v1→v2 已踩过这个坑）
- **MCP 服务器配置**：codeg 的 MCP 配置写在各智能体 CLI 自己的文件里（`~/.claude.json`、`~/.gemini/settings.json` 等，见 `commands/mcp.rs:731+`），不是 codeg 拥有的数据。同步它等于替用户改写第三方 CLI 配置，风险不对等，v1 不做。
- `tokens.json`、`github_accounts`（OAuth 凭据，与设备/授权流绑定）
- `system_proxy_settings`（每机每网络各不相同，推过去无意义且多一处泄露面）、`system_terminal_settings`（本机 shell 绝对路径）、`web_service_*`（端口/令牌是本机服务配置）
- `remote_workspace_connection`（含长期令牌，且多设备互相覆盖连接表的语义不清；v1 不做，留待后续单独评估）
- **WebDAV 同步配置本身**（见 R5）

### R2 可移植偏好白名单

`app_metadata` 是杂物袋，必须是**允许清单**而不是排除清单——将来任何新键默认不同步，避免有人加了个设备本地键就被静默推到别的机器。

v1 白名单（实现时以代码中的常量为准，此处为意图说明）：
- 语言/外观类：`system_language_settings`、`appearance_mode`、`zoom_level`
- 智能体工具开关：delegation 系列、feedback / question / session-info 开关
- 聊天通道的非凭据配置：命令前缀、消息语言
- 日志级别

应用侧按白名单**叠加**而非整域覆盖：快照里的键覆盖本机同名键，白名单之外的本机键原样保留。

### R3 远端布局与上传顺序

```
{remoteDir}/v1/{profile}/
  ├── manifest.json   # 元信息 + config.json 的 size 与 sha256
  └── config.json     # 快照本体
```

默认 `codeg/v1/default/`。版本段 `v1` 夹在中间，让用户在网盘客户端里看到干净的顶层目录；协议不兼容演进时换该段。schema 的兼容演进走 manifest 的 `schemaVersion`：新客户端可读旧快照，旧客户端拒绝新快照并提示升级。`profile` 支持同账号隔离多套配置。

- **上传顺序固定为「先 PUT config.json，再 PUT manifest.json」**。manifest 是「这份备份可用」的信号，最后写。中途失败时远端是旧 manifest + 新 config，下载侧的 sha256 校验会拦下不一致，而不会把残缺配置当合法快照应用。
- 所有远端读写由一个进程内 mutex 串行化——上传是两步 PUT，并发会让两个文件来自不同快照。

### R4 传输层

- 方法：`PROPFIND`（Depth=0 探活，不解析 XML）、`MKCOL`（逐级创建远端目录）、`PUT`、`GET`。
- 分级超时：探活短、上传/下载长。
- 响应体大小上限，防止恶意/异常远端撑爆内存。
- **日志脱敏**：URL、账号、密码、Authorization 头一律不得进日志。
- 针对常见网盘（坚果云等）的 HTTP 状态码给定向错误文案（如 401 提示需用应用密码而非登录密码）。

### R5 同步配置与凭据

- 同步配置（`enabled`、`serverUrl`、`username`、`password`、`remoteDir`、`profile`、`autoSync`、`intervalMinutes`）独立持久化，**绝不进入快照白名单**。若随快照流转，A 机器的凭据会覆盖 B 机器，形成同步循环。
- 读取配置回传前端的视图**不含密码**，只用 `hasPassword: bool` 告知是否已设置。
- **密码回填**：密码框的契约是「**留空则不修改**」——已保存时输入框显示占位提示「已保存，留空则不修改」，提交空串即表示沿用库里旧值。
  - 不采用「掩码占位符」方案。LiveAgent 记录的真实 bug：UI 给密码框填掩码字符后原样提交，把掩码当新密码写库，下次同步认证失败。issue 截图里那一版已经改成了「留空则不修改」。
  - 后端参数用 `password: Option<String>`，`None` 或空串一律沿用旧值。只有一个机制，不再额外加 `passwordTouched` 标志位。

### R6 自动同步

- **只上传，不下载。** 自动拉取会在用户毫无察觉时覆盖本机配置，出错方向不可接受；拉取永远是手动动作。
- **触发方式：周期性快照哈希比对**（默认 5 分钟，可配）。到点采集快照、算 sha256，与「上次成功上传的 sha256」不同才上传，相同则完全跳过网络请求。
  - 不采用 LiveAgent 的「写入咽喉标脏 + 防抖」：LiveAgent 有 6 个集中的 `save_*` 函数作为天然咽喉，codeg 的配置写入分散在几十处命令里，逐处插桩既易漏又是大面积改动。快照只有几十 KB，周期性采集 + 哈希的成本可忽略，且天然不会漏。
- 启动后延迟一段时间（避开启动风暴）做首次比对。
- **抑制**：下载并应用远端快照期间不得触发自动上传（应用快照本身会改本地配置，不抑制就会把刚拉下来的数据原样推回去）。本地文件导入路径**有意不抑制**——用户主动导入的配置应当传播到远端。
- 自动同步失败不得打扰用户：记录 `lastError`，通过事件推给设置页展示，不弹通知。连续失败不做指数退避以外的处理，不禁用配置。

### R7 命令与前端

命令（桌面 Tauri 命令；采集/应用逻辑写成 `_core` 形式，遵循 codeg 双模式约定，便于后续给 Web 模式复用）：

| 命令 | 说明 |
|---|---|
| `config_sync_export` / `config_sync_peek_import` / `config_sync_apply_import` | 本地文件导入导出；peek 只解析校验不写入，供确认对话框展示来源设备与各域条目数 |
| `config_sync_load_settings` / `config_sync_save_settings` | 同步配置存取（密码按 R5 处理） |
| `config_sync_test_connection` | PROPFIND Depth=0 探活 |
| `config_sync_fetch_remote_info` | 只拉 manifest，供上传/下载前确认对话框；远端无备份返回 `null` |
| `config_sync_upload` / `config_sync_download` | 手动同步 |

- 后台自动同步结果通过事件推送，载荷 `{ lastSyncAt, lastError }`。手动同步的成败由命令返回值同步告知，不走事件——收到事件即意味着「后台自动同步」。
- UI 落在备份设置页（`src/components/settings/backup-settings.tsx`）新增分区，或新建同级分区，与既有 `SettingCard` / `SettingRow` 风格一致。
- **必须有一张「同步范围」卡**，用「包含 / 不包含」两栏把 R1 的范围决策直接摆在界面上（包含：服务商、智能体设置、自定义智能体、快捷消息、任务模板、可移植偏好；不包含：对话历史、上传文件、工作区项目、MCP 配置、凭据）。issue 截图里就有这张卡——它是让用户不误判「自动备份 = 备份一切」的关键，比任何说明文字都直观。
- **服务商预设**：提供「坚果云 / Nextcloud / 群晖 NAS / 自定义」快捷入口，选中后预填服务器地址模板与对应的帮助文案（如坚果云需用应用密码）。纯前端预填，不引入任何服务商专属逻辑。
- 「账号密码仅保存在本机」的说明需在 WebDAV 卡片上直接可见。
- 分区文案还须写明：同步是**合并**不是镜像——本地多出来的条目不会被远端快照删除。
- 应用远端/导入快照前自动在本地留一份当前配置快照（复用导出逻辑写到 `~/.codeg/config-snapshots/`），出错可回退。

### R8 国际化

所有新增文案进入 `src/i18n/messages/*.json` 全部 10 种语言，不留缺键。

## Acceptance Criteria

- [ ] 未配置 WebDAV 时，设置页分区显示为未启用态，不发起任何网络请求
- [ ] 填入合法 WebDAV 地址/账号/密码后「测试连接」成功；地址错误、账号错误、目录不存在各自给出可区分的错误文案
- [ ] 手动「上传」后，用第三方 WebDAV 客户端能在 `codeg/v1/default/` 下看到 `config.json` 与 `manifest.json`，`manifest.json` 中的 sha256 与 size 与 `config.json` 实际值一致
- [ ] 在 B 机器（或清空配置的测试库）上手动「下载」后，服务商、智能体设置、自定义智能体、快捷消息、任务模板、白名单偏好全部还原
- [ ] 下载应用前本地留下了当前配置的回退快照
- [ ] 快照 JSON 中**不含** WebDAV 服务器地址、账号、密码
- [ ] 快照 JSON 中不含对话历史、`folder` 记录、`system_proxy_settings`、`installed_version`、`skills_dir`
- [ ] 白名单之外的 `app_metadata` 键在应用快照后保持本机原值（叠加而非覆盖整域）
- [ ] 手动篡改远端 `config.json` 使 sha256 不匹配后，下载被拒绝并给出校验失败提示，本地配置未被改动
- [ ] 开启自动同步后，改动一个服务商配置，在一个周期内远端 `config.json` 被更新；不做任何改动时下一周期不产生 PUT 请求
- [ ] 正在应用下载的快照期间不会触发自动上传
- [ ] 断网状态下自动同步失败，不弹通知，设置页显示上次错误，恢复网络后下一周期自动成功
- [ ] 前端未改密码直接保存同步配置，密码不被掩码占位符覆盖，下次同步仍成功
- [ ] 日志中不出现 WebDAV 密码、Authorization 头或带凭据的完整 URL
- [ ] 旧 schemaVersion 的快照可被当前版本读取；更高 schemaVersion 的快照被拒绝并提示升级
- [ ] `pnpm eslint .` / `pnpm test` / `pnpm build` 全绿
- [ ] `cargo test --features test-utils` / `cargo clippy --all-targets --features test-utils -- -D warnings` 全绿
- [ ] `cargo check --no-default-features --bin codeg-server` 全绿
- [ ] 10 种语言消息文件均含新增键

## 非目标

- 不加密快照（v1）；manifest 预留 `encryption` 字段留升级路径
- 不做全量归档（数据库 / uploads / 会话记录）的自动上传
- 不做自动下载、不做双向合并、不做冲突解决 UI
- 不同步 MCP 配置（属第三方 CLI 文件）
- 不做除 WebDAV 外的其他远端后端（S3、Git 等）

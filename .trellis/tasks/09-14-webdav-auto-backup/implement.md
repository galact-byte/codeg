# 执行计划：WebDAV 配置同步

按 Red → Green → Refactor 推进。阶段之间是可提交的回滚点。

## 阶段 0 · 摸底确认（无产出，但必须做完）

- [ ] 确认 `agent_setting` 表的唯一键定义（`src-tauri/src/db/migration/m20260226_000001_agent_setting.rs`）
- [ ] 确认 `model_provider` 无唯一索引，`(agent_type, name)` 作为自然键在现有数据中是否真的唯一；若不唯一，改用「name 优先 + 冲突时按 api_url 区分」并把结论写回 design.md §4
- [ ] 列出 `app_metadata` 中实际在用的全部键（`grep -rn "_KEY: &str" src-tauri/src`），逐个判定进不进白名单，把结果固化为 `portable_keys.rs` 的常量数组
- [ ] 确认 `~/.codeg/config-snapshots/` 不落在任何 `MANAGED_SECTIONS` 的 live path 下（`commands/backup/sections.rs`）

## 阶段 1 · RED → GREEN：快照采集与应用（不碰网络）

先把最有价值也最危险的部分做完并测透，网络层是后面的事。

- [ ] RED：写 design.md §11 的测试 1–7，此时模块不存在
- [ ] `commands/config_sync/domains.rs`：`ConfigDomain` 结构 + `CONFIG_DOMAINS` 表 + 一致性测试
- [ ] `commands/config_sync/portable_keys.rs`：白名单常量 + 「与凭据键交集为空」测试（测试 10）
- [ ] `commands/config_sync/snapshot.rs`：`collect_snapshot_core` / `apply_snapshot_core` / `validate_manifest`
  - 采集时剔除黑名单字段（`installed_version`、`skills_dir`、`id`、时间戳）
  - `agentSettings` 记录所属服务商的**自然键**而非数字 id；应用时查本地 id 回填
  - 应用全程单事务，任一域失败整体回滚
  - upsert 语义：命中自然键则更新，未命中则插入，**不删本地多余行**
- [ ] 回退快照：应用前写 `~/.codeg/config-snapshots/<rfc3339>.json`，保留最近 10 份

验证：`cd src-tauri && cargo test --features test-utils config_sync`

## 阶段 2 · RED → GREEN：本地导入导出

- [ ] `commands/config_sync/local_io.rs` + 命令 `config_sync_export` / `config_sync_peek_import` / `config_sync_apply_import`
- [ ] `peek` 只解析校验不写入，返回来源设备与各域条目数
- [ ] schemaVersion 更高 → 拒绝并给升级提示（测试 8 的一半）

验证：`cargo test --features test-utils`、`cargo clippy --all-targets --features test-utils -- -D warnings`

## 阶段 3 · RED → GREEN：WebDAV 传输层

- [ ] RED：测试 11（错误脱敏）
- [ ] `src-tauri/src/network/webdav.rs`：`probe` / `ensure_dir` / `put` / `get`
  - `PROPFIND` / `MKCOL` 用 `Method::from_bytes` 构造，**不解析 XML**
  - 分级超时（探活 15s / 传输 120s）、响应体上限
  - 错误类型的 `Display` 与所有日志只出现 `{method} {rel} -> {status}`；禁止 base URL / 账号 / 密码 / Authorization
  - 401 / 403 / 404 / 409 / 507 各给独立 i18n key
- [ ] 路径分段校验：`remoteDir`、`profile` 非空且不含 `/` `\` `..`

验证：`cargo test --features test-utils webdav`、`cargo clippy ... -D warnings`

## 阶段 4 · RED → GREEN：同步配置与编排

- [ ] RED：测试 9（密码回填）、测试 8 的另一半（sha256 不匹配拒绝且本地未改动）
- [ ] `ConfigSyncSettings` 存储态 / `ConfigSyncSettingsView` 视图态（无 password，只有 `has_password`）
- [ ] 命令 `config_sync_load_settings` / `config_sync_save_settings`（`password: Option<String>`，`None` 与空串均沿用旧值）
- [ ] `commands/config_sync/webdav_sync.rs`：
  - `upload_core`：采集 → 算 sha256 → `ensure_dir` → **先 PUT config.json，再 PUT manifest.json**
  - `download_core`：GET manifest → GET config → 校验 sha256 与 size → 不匹配则拒绝
  - 全部远端读写走一个进程内 `tokio::sync::Mutex`
- [ ] 命令 `config_sync_test_connection` / `config_sync_fetch_remote_info` / `config_sync_upload` / `config_sync_download`

验证：`cargo test --features test-utils`、`cargo clippy ... -D warnings`

## 阶段 5 · RED → GREEN：自动同步

- [ ] RED：测试 12（无变化零上传，用假传输层断言调用次数）、测试 13（抑制守卫）
- [ ] `commands/config_sync/auto_sync.rs`：
  - 周期任务：采集 → sha256 → 与持久化的 `last_uploaded_sha256` 比对 → 相同则跳过
  - 启动延迟 60s 首跑
  - 失败退避 `interval * 2^min(n,4)`，成功归零；不禁用配置、不弹通知
  - `AutoSyncSuppressionGuard` RAII 引用计数；`download` 期间持有，**本地导入路径不持有**
  - 结果事件 `config-sync://status`，载荷 `{ lastSyncAt, lastError }`
- [ ] 在 `lib.rs` 启动流程中挂起该任务（`tauri-runtime` 下）

验证：`cargo test --features test-utils`、`cargo clippy ... -D warnings`、`cargo check --no-default-features --bin codeg-server`

## 阶段 6 · RED → GREEN：前端

- [ ] RED：vitest 测试 14、15
- [ ] `src/lib/config-sync.ts`：命令封装 + 事件类型；`src/lib/types.ts` 补类型镜像
- [ ] `src/components/settings/config-sync-settings.tsx`：
  - 「本地备份」组：导出 / 导入（导入前用 peek 结果弹确认）
  - 「WebDAV 同步」组：地址 / 账号 / 密码 / 远端目录 / profile / 自动同步开关 / 周期；测试连接、立即上传、立即下载
  - 密码框始终空值渲染，已保存时占位提示「已保存，留空则不修改」
  - 「同步范围」卡：包含 / 不包含 两栏，把 PRD R1 的范围决策摄在界面上
  - 服务商预设：坚果云 / Nextcloud / 群晖 NAS / 自定义，纯前端地址模板 + 帮助文案，不引入后端专属逻辑
  - 显示 `lastSyncAt` / `lastError`
  - **文案必须明确写出**：同步配置，不含对话历史与上传文件；同步是合并不是镜像（本地多余项不会被删除）
- [ ] 与既有 `backup-settings.tsx` 的 `SettingCard` / `SettingRow` 风格保持一致

验证：`pnpm test`、`pnpm eslint .`

## 阶段 7 · 国际化

- [ ] 新增 `ConfigSyncSettings` 命名空间，含全部 WebDAV 错误码文案
- [ ] 10 个语言文件全部补齐：`en / zh-CN / zh-TW / ja / ko / es / de / fr / pt / ar`
- [ ] 脚本比对键集合一致

验证：`pnpm test`、`pnpm build`

## 阶段 8 · REFACTOR + 全量验证

- [ ] 通读 diff；注释只解释「为什么」
- [ ] 全量验证：
  - `pnpm eslint .` / `pnpm test` / `pnpm build`
  - `cd src-tauri && cargo test --features test-utils`
  - `cd src-tauri && cargo clippy --all-targets --features test-utils -- -D warnings`
  - `cd src-tauri && cargo check --no-default-features --bin codeg-server`
- [ ] 对真实网盘（坚果云或自建）手工走完 prd.md 验收清单
- [ ] 在配置好同步的机器上抓一次日志，确认无凭据泄漏

## 回滚点

| 点位 | 状态 |
|---|---|
| 阶段 2 结束 | 本地导入导出可独立发布，无网络能力 |
| 阶段 4 结束 | 手动 WebDAV 同步可用，无后台任务 |
| 阶段 5 结束 | 完整功能 |

## 风险与守则

- **快照绝不能带上 WebDAV 凭据。** 这是唯一的「一旦发生就自我放大」的故障（同步循环 + 凭据扩散）。白名单交集测试是硬门槛。
- **不删本地多余行**是有意取舍，不要在实现时「顺手」改成清表重插。
- **不新增 crate 依赖**；`reqwest` / `sha2` / `serde_json` 已在。
- **不改动现有备份归档引擎**（`commands/backup/`）的任何行为。
- 采集/应用逻辑写成 `_core`，不要把 `AppHandle` / `State` 渗进去。
- 域表加了新域必须同时接上应用器——一致性测试会拦，但不要靠 `#[ignore]` 绕过。

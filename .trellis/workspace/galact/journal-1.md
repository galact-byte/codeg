# Journal - galact (Part 1)

> AI development session journal
> Started: 2026-09-14

---

## 2026-09-14 · close-behavior-choice（issue #633 子任务一）

主窗口关闭按钮行为改为三值偏好 `ask | minimize | exit`，默认 `ask`。

### 关键决策

- **偏好由后端启动时从库里播种**，不走前端 push（LiveAgent 的做法有竞态窗口）。落点与 `apply_persisted_terminal_settings` 同一处。
- **`can_hide_to_tray()` 在读取偏好之前短路**：托盘不可用时隐藏主窗会让 `skip_taskbar` 窗口（桌宠等）变成孤儿，属平台能力约束，偏好不能覆盖。
- **关闭是请求/应答握手**：`ask` 模式下前端对话框必须最终调用 `resolve_close_request`（Esc / 取消传 `"cancel"`），否则后端待处理标志保持置位，关闭按钮永久失效。
- 偏好读取路径永不返回 `Err`，脏数据一律回退 `ask`——关闭是用户最后的退路。
- 对话框事件用 `window.emit` 只发主窗，挂在 `src/app/workspace/layout.tsx` 而非根 layout（根 layout 被桌宠/设置/面板多个 webview 共用，广播会同时弹出多个框）。

### 顺带修复（超出任务范围，已披露）

`terminal/manager.rs` 的 `reap_exited` 现在对回收的终端调用 `cleanup_temp_files`——此前移除实例只释放 PTY，凭据文件和辅助脚本留在磁盘上。

### 本机验证踩坑（Windows 专有）

1. **`cargo test` 生成的测试 exe 起不来**：`STATUS_ENTRYPOINT_NOT_FOUND`。crate 链接了 `TaskDialogIndirect` / `SetWindowSubclass`，只有 comctl32 **v6** 导出；正式 `codeg.exe` 靠 tauri-build 注入的清单绑到 v6，测试 exe 没有清单 → 落到 System32 的 v5 → 进程加载即挂。CI 也承认这点（`test.yml` 里 Windows desktop 格用 `--no-run`）。
   绕法：`mt.exe -manifest <Common-Controls 6.0 清单> -outputresource:<exe>;#1` 事后嵌清单，即可执行。
2. **默认并行度会撑爆页面文件**（os error 1455 / `STATUS_STACK_BUFFER_OVERRUN`）：14 个目标同时 mmap 巨型 rlib。降到 `-j 2` 稳定。
3. `commands::acp::tests::deepseek_project_skills_hang_off_the_git_root` 在本机恒失败——它断言"往上找不到 `.git` 时回退工作区自身"，但 `C:\Users\g1582\.git` 是个仓库，临时目录在其下。纯环境问题。

### 验证结果

- 桌面 lib 单测 3539 passed / 1 failed（上述环境项）；14 个集成测试二进制全绿。
- clippy 桌面 `--all-targets --features test-utils -D warnings` 通过；服务器 `--no-default-features --bin codeg-server --lib -D warnings` 通过。
- 前端 `tsc --noEmit` / `vitest`（437 文件 6344 测试）/ `pnpm build` 全绿。

### Spec 判断

`.trellis/spec/` 当前只有 frontend 与 guides 两层，本次未产生新的前端约定（设置分区组件 + 对话框 + api 层都沿用既有模式），Windows 测试清单的坑属构建环境知识、无对应 spec 落点。结论：本次无需更新 spec。

## 2026-09-15 · webdav-config-sync（issue #633 子任务二）

配置数据（服务商含密钥 / 智能体设置 / 自定义智能体 / 快捷消息 / 任务模板 / 可移植偏好）打包成几十 KB 的 JSON 快照，支持本地导入导出与 WebDAV 同步。27 文件 5656 行，纯净分支 `feat/webdav-config-sync`（基于 upstream f4f34180，单提交）。

### 关键决策

- **只自动上传，从不自动下载**。定时拉取等同于另一台机器静默覆盖本机设置；每次下载都是显式动作 + 确认框（先 `peek_remote` 拿 manifest 展示来源设备/时间/条目数，确认后才 `download_apply`）。
- **不复用备份引擎**（`commands::backup`）。那条链路是 GB 级加密归档，含会话、上传文件、transcript，不是"每 5 分钟跑一次"的单位。安全边界是用户自有的、已认证的 WebDAV 端点，所以快照明文，API key 明文走，而同步自身的凭据被排除在快照之外——否则两台机器会互相覆盖凭据变成同步环。
- **自动触发用周期性哈希比对**（默认 5 分钟采集快照算 sha256，与 `lastUploadedSha256` 不同才上传），没有学 LiveAgent 的"写入咽喉标脏 + 防抖"：codeg 的配置写入散在几十个命令里，没有单一咽喉。手动「立即同步」绕过哈希抑制。
- **按自然键 upsert，不删除本地多余行**。清表重插会重排自增主键，打断 `agent_setting.model_provider_id` 这类外键。代价是远端删除不向本地传播，已在 UI 文案里写明。
- **排除 `work_task_settings`**：其 `folder_id` 外键挂在设备本地的项目记录上，跨机器会指向错误项目；只同步 `work_task_template`。
- **`preferences` 用允许清单而非排除清单**（`portable_keys::PORTABLE_PREFERENCE_KEYS`），新 key 默认不参与同步，由白名单交集测试守护凭据不外流。
- **密码编辑只有一个机制**：留空 = 沿用旧值。没有 `password_touched` 之类的第二真相源，视图只回 `hasPassword: bool`。
- **远端两步写入**：先 `config.json` 后 `manifest.json`。WebDAV 没有多文件事务，上传中断必须让旧 manifest 指向一致的字节。

### 真机验证（坚果云）

用户用坚果云 `https://dav.jianguoyun.com/dav/` 跑通全流程，日志轨迹：

- 测试连接 `PROPFIND /dav/codeg/v1/default → 404`：目录尚不存在，视为连接成功（目录首次上传时创建）。
- 上传逐级 `MKCOL /codeg → /codeg/v1 → /codeg/v1/default`，各返 **201**——证实每级此前都不存在，而 RFC 4918 规定中间集合缺失时 `MKCOL` 返回 409，因此 `ensure_dir` 的逐级实现是必需的，不可简化。
- `PUT config.json 201` → `PUT manifest.json 201`，顺序正确。
- 从远端恢复：`GET manifest 200`（确认框预览）→ 确认后 `GET manifest 200` + `GET config 200`，校验和通过并应用。

### 本轮发现的缺陷（已修）

URL 拼接双斜杠：base 自带尾斜杠时再 push 空段会产出 `/dav//codeg/v1/...`，已修并加测试断言尾斜杠有无都等价。

### 验证结果

- 前端 `vitest` 439 文件 6360 用例全绿（含 16 个新增 config-sync 测试）；`tsc --noEmit`、eslint、`next build` 静态导出全过；i18n 一致性 19 用例通过（10 语言 key 对齐）。
- 桌面 clippy `--all-targets --features test-utils -D warnings` 通过，lib 测试 3576 passed（唯一失败仍是环境项 `deepseek_project_skills_hang_off_the_git_root`）；server / mcp clippy 通过。

### Spec 更新

本次触发 code-spec 强制条件（新命令签名 + 跨层契约 + 外部存储与凭据集成），新建 `.trellis/spec/backend/` 层：

- `backend/index.md` — 后端层入口，记录 `_core` 分层、feature gating、错误类型三条通用约定。
- `backend/config-sync-contract.md` — 七节完整契约：命令签名表、远端布局与写入顺序、settings 输入/视图契约、错误矩阵（含 i18n key 与 `WebdavError → AppCommandError` 映射）、Good/Base/Bad、必需测试断言点、Wrong vs Correct（逐级 MKCOL、禁止定时下载）。
- `guides/cross-layer-thinking-guide.md` 增补「Cross-Machine Config Boundary」检查项：新配置字段是否该跨机器同步、是否进白名单、是否有反向测试、应用两次是否幂等。


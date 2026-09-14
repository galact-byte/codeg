# 执行计划：主窗口关闭行为可配置

按 Red → Green → Refactor 推进。每个阶段结束跑该阶段的验证命令，全绿再进下一阶段。

## 阶段 0 · 摸底确认（无产出）

- [ ] 确认 `src-tauri/src/lib.rs` 中 `label == "main"` 的 `CloseRequested` 分支行号（设计文档记的 `:1149` 是撰写时的位置，可能已漂移）
- [ ] 确认 `apply_persisted_terminal_settings` 的启动播种块位置（`:625` 一带）
- [ ] 确认托盘退出菜单分支 `windows::TRAY_MENU_ID_QUIT => app.exit(0)`（`:1012` 一带）保持不变
- [ ] 读 `terminal/manager.rs:452` 的 `list_with_exit_check` 与 `:495` 的 `kill_by_owner_window`，确认两者用的是同一套存活/归属判定，计数方法能直接复用

## 阶段 1 · RED：后端偏好模型的失败测试

- [ ] 在 `src-tauri/src/commands/system_settings.rs` 的 `#[cfg(all(test, feature = "tauri-runtime"))]` 测试模块中新增 4 个测试（对应 design.md §8 的 1–4），此时被测函数尚不存在，编译失败即为 RED

验证：`cd src-tauri && cargo test --features test-utils close_behavior`（预期编译失败）

## 阶段 2 · GREEN：后端偏好模型

- [ ] `src-tauri/src/models/system.rs`：新增 `CloseWindowBehavior` 与 `SystemCloseBehaviorSettings`，cfg-gate 到 `tauri-runtime`，在 `models/mod.rs` 导出
- [ ] `src-tauri/src/commands/system_settings.rs`：
  - 常量 `SYSTEM_CLOSE_BEHAVIOR_SETTINGS_KEY = "system_close_behavior_settings"`
  - `load_system_close_behavior_settings(conn)` —— 解析失败/未知值一律 `warn!` + 默认 `Ask`，**永不返回 Err**
  - `CLOSE_BEHAVIOR_CACHE: AtomicU8` + `cached_close_behavior()` + 内部写缓存函数
  - `apply_persisted_close_behavior(conn)` 启动播种
  - 命令 `get_system_close_behavior_settings`（返回体含 `tray_available`）与 `update_system_close_behavior_settings`（先写库、成功后刷缓存）
- [ ] 在 `lib.rs` 的 `invoke_handler!` 注册两个命令

验证：`cargo test --features test-utils close_behavior` 全绿

## 阶段 3 · GREEN：关闭分支与启动播种

- [ ] `terminal/manager.rs`：新增同步 `count_live_by_owner_window(&self, label: &str) -> usize`，复用 `list_with_exit_check` 的存活判定；配一条回归断言它与 `kill_by_owner_window` 的返回值一致（design.md §8 测试 5）
- [ ] `lib.rs` 启动块：在终端设置播种块之后，照同样的 `block_on` 模式加一次 `apply_persisted_close_behavior`
- [ ] `lib.rs` `CloseRequested` 主窗口分支按 design.md §4 改造：先把托盘不可用折叠为 `Exit`（**不要提前 `exit(0)` 并 return**，否则该平台绕过终端确认），再按三分支 + 终端计数分发
- [ ] 新增 `CLOSE_PROMPT_OPEN: AtomicBool`（放 `system_settings.rs` 与缓存同处，或 `windows.rs`，二选一但只此一处）
- [ ] 新增命令 `resolve_close_request(action, remember)`，注册进 `invoke_handler!`
- [ ] **事件用 `window.emit(...)` 只发给主窗口**，不要用 `app.emit(...)`。根布局被所有 webview（pet / settings / pet-panel）共用，广播会在多个窗口同时弹框
- [ ] 事件载荷为 `{ mode, runningTerminals }`；`confirm-terminals` 与 `ask` 共用同一个 `CLOSE_PROMPT_OPEN` 去重位与同一个 `resolve_close_request` 命令，不另开事件或命令

验证：
- `cargo clippy --all-targets --features test-utils -- -D warnings`
- `cargo check --no-default-features --bin codeg-server`（确认所有新代码正确 cfg 隔离）

## 阶段 4 · RED → GREEN：前端对话框

- [ ] 先写 vitest（design.md §8 的 5–7），此时组件不存在 → RED
- [ ] `src/lib/types.ts` 镜像 `CloseWindowBehavior` / `SystemCloseBehaviorSettings`（含 `trayAvailable`）
- [ ] `src/lib/api.ts` 新增三个封装：读设置、写设置、`resolveCloseRequest`
- [ ] `src/components/close-behavior/close-request-dialog.tsx`：
  - 监听 `app://close-request`；非 Tauri 环境（`isDesktop()` 为假）直接返回 `null`，不注册监听
  - `AlertDialog` + `Checkbox`；`onOpenChange(false)` → `resolveCloseRequest("cancel", false)`
  - 两种形态：`ask` 显示两个动作 + 「记住我的选择」，`runningTerminals > 0` 时描述内联终端警告；`confirm-terminals` 只显示「取消 / 确认退出」不带勾选框
  - 组件 unmount 时若对话框仍开着，补发一次 `cancel` 复位后端 `CLOSE_PROMPT_OPEN`
- [ ] 挂载到 `src/app/workspace/layout.tsx`（主窗口专属布局），**不要挂 `src/app/layout.tsx`**

验证：`pnpm test` 全绿

## 阶段 5 · GREEN：设置页入口

- [ ] `src/components/settings/general-settings.tsx` 新增 `SettingRow` + `Select` 三选一
  - 复用该文件既有的 `loading` / `saving` / `loadError` 结构，不另起一套状态机
  - 桌面态判断沿用同文件里 `isDesktop() && getActiveRemoteConnectionId() === null` 的先例
  - `trayAvailable === false` 时 `disabled` 并显示解释文案（不要在前端用 `isLinux` 猜）

验证：`pnpm eslint .`、`pnpm build`

## 阶段 6 · 国际化

- [ ] 在 `GeneralSettings` 命名空间下加设置项文案，新建 `CloseRequestDialog` 命名空间放对话框文案
- [ ] 10 个语言文件全部补齐：`en / zh-CN / zh-TW / ja / ko / es / de / fr / pt / ar`
- [ ] 用脚本比对键集合一致，确认无缺键

验证：`pnpm test`、`pnpm build`

## 阶段 7 · REFACTOR + 全量验证

- [ ] 通读 diff，去掉调试痕迹与冗余注释；注释只解释「为什么」
- [ ] 全量验证：
  - `pnpm eslint .`
  - `pnpm test`
  - `pnpm build`
  - `cd src-tauri && cargo test --features test-utils`
  - `cd src-tauri && cargo clippy --all-targets --features test-utils -- -D warnings`
  - `cd src-tauri && cargo check --no-default-features --bin codeg-server`
- [ ] 按 prd.md 验收清单逐条手工走一遍（需要真实启动桌面应用）

## 回滚点

| 点位 | 回滚方式 |
|---|---|
| 阶段 2 结束 | 后端偏好模型独立可回滚，无行为变化（尚未接入关闭分支） |
| 阶段 3 结束 | 行为已变化；回滚需一并撤销 `lib.rs` 两处改动 |
| 阶段 5 结束 | 功能完整，可作为提交点 |

## 风险与守则

- **不得改动 `can_hide_to_tray()` 的语义或它在关闭分支中的短路位置。** 它挡的是「隐藏主窗口会孤儿化 `skip_taskbar` 的桌宠窗口」这个真实缺陷，不是保守设定。
- **不得新写一套退出清理逻辑。** `exit` 分支只调 `app.exit(0)`，复用既有 `APP_QUITTING` → 清理块的路径。
- **不得让偏好读取路径返回 Err。** 关闭按钮是用户最后的出口。
- 改动集中在上述文件；不顺手重构 `general-settings.tsx` 或 `lib.rs` 的其他部分。

# 技术设计：主窗口关闭行为可配置

## 1. 边界

| 层 | 文件 | 改动性质 |
|---|---|---|
| 偏好模型 | `src-tauri/src/models/system.rs` | 新增 `CloseWindowBehavior` 枚举 + `SystemCloseBehaviorSettings` 结构 |
| 偏好读写 + 原子缓存 | `src-tauri/src/commands/system_settings.rs` | 新增加载/保存/缓存函数与两个 Tauri 命令 |
| 关闭分支 | `src-tauri/src/lib.rs`（`main` 的 `CloseRequested` 分支，当前 `:1149` 一带） | 改造既有二分支为三分支 |
| 存活终端计数 | `src-tauri/src/terminal/manager.rs` | 新增一个按 owner window 计数的同步方法 |
| 启动播种 | `src-tauri/src/lib.rs`（`apply_persisted_terminal_settings` 播种块附近，`:625` 一带） | 新增一次 `block_on` 播种 |
| 询问对话框 | `src/components/close-behavior/close-request-dialog.tsx`（新建） | 新组件 |
| 挂载点 | 主工作区根布局（与既有全局对话框同级） | 挂载新组件 |
| 设置项 | `src/components/settings/general-settings.tsx` | 新增一个 `SettingRow` |
| 前端 API | `src/lib/api.ts` / `src/lib/types.ts` | 新增命令封装与类型镜像 |
| 文案 | `src/i18n/messages/*.json`（10 个） | 新增键 |

## 2. 数据模型

```rust
// models/system.rs — 与 SystemRenderingSettings 同样 cfg-gate 到 tauri-runtime
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CloseWindowBehavior {
    #[default]
    Ask,
    Minimize,
    Exit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SystemCloseBehaviorSettings {
    pub behavior: CloseWindowBehavior,
}
```

持久化键：`system_close_behavior_settings`，值为 `{"behavior":"ask"}` 形式的 JSON，落在 `app_metadata`。

**解析失败 = 默认值。** 与 `load_system_language_settings` 不同（那里返回错误），这里必须永不失败：关闭按钮是最后一道用户出口，一个坏掉的 JSON 不能让人关不掉窗口。`serde_json::from_str(...).unwrap_or_default()` + `tracing::warn!`。同理，`#[serde(other)]` 不适用于 unit-only enum 的 rename_all 场景，改为在解析失败时整体回落到默认值即可覆盖「未知字符串」情形。

## 3. 关闭事件是同步上下文 → 原子缓存

`WindowEvent::CloseRequested` 的回调是同步闭包，不能 `await` 数据库，也不适合 `block_on`（会在 UI 线程上阻塞 SQLite）。

沿用 LiveAgent 的做法但简化：进程内一个静态原子。

```rust
// system_settings.rs
#[cfg(feature = "tauri-runtime")]
static CLOSE_BEHAVIOR_CACHE: AtomicU8 = AtomicU8::new(BEHAVIOR_ASK);

pub fn cached_close_behavior() -> CloseWindowBehavior;   // 同步读，供 lib.rs
fn store_close_behavior_cache(b: CloseWindowBehavior);   // 同步写
pub async fn apply_persisted_close_behavior(conn: &DatabaseConnection);  // 启动播种
```

与 LiveAgent 的区别：LiveAgent 由**前端**在启动时推送偏好给后端（`app_set_close_window_behavior`），后端不读库。这在前端尚未 ready 时留了一个窗口期，行为落回默认。codeg 改为**后端启动时直接从库播种**，与 `apply_persisted_terminal_settings` 同一模式、同一位置，不依赖前端生命周期。

数据库是唯一真相源，原子只是热路径缓存：更新命令先写库，写成功后再刷新原子；写库失败则不动原子，UI 收到错误。

## 4. 关闭分支改造

`lib.rs` 中 `label == "main"` 的 `CloseRequested`，`!APP_QUITTING` 分支改为：

```
api.prevent_close();
// 托盘不可用时偏好被强制折叠为 Exit，而不是提前 return——
// 这样它也能走下面的终端确认守卫。
let behavior = if windows::can_hide_to_tray() {
    cached_close_behavior()
} else {
    CloseWindowBehavior::Exit
};
let running = terminal_count_for_main(app);   // 同步，下文 §4.1
match behavior {
    Minimize => { let _ = window.hide(); }
    Exit if running == 0 => { app.exit(0); }
    // 有终端在跑 → 先确认，与 Ask 共用一个事件与一个去重门
    Exit => emit_close_request(app, Mode::ConfirmTerminals, running),
    Ask  => emit_close_request(app, Mode::Ask, running),
}
return;
```

要点：
- `can_hide_to_tray()` 的判定**在偏好之前**。这是平台能力约束，不是用户偏好能推翻的（现有注释已说明理由）。但它现在是「把偏好折叠成 `Exit`」而不是「直接 `exit(0)` 并 return」，否则托盘不可用的平台就绕过了终端确认——而那恰恰是最容易意外丢终端的平台。UI 侧依旧禁用该项。
- `exit` 分支就是 `app.exit(0)`，与托盘菜单「退出 Codeg」（`lib.rs:1012`）走完全相同的路径 → `ExitRequested` 置 `APP_QUITTING` → 再次进入本分支时落到既有的 ACP 断连 / 终端回收清理块。不新增清理代码。

### 4.1 存活终端计数

`TerminalManager` 已有 `list_with_exit_check`（`terminal/manager.rs:452`）承担存活判定。新增一个 `count_live_by_owner_window(&self, label: &str) -> usize` 复用同一判活口径，**不另写一套**（否则确认框说有 3 个、实际杀了 2 个）。该方法必须是同步的，因为关闭回调是同步上下文。

只数终端，不数 ACP 连接：断开连接不丢数据（会话转录已落盘），而杀终端会直接终结用户正在跑的进程。把两者混在一起只会让确认框常驻、进而被无视。
- `CLOSE_PROMPT_OPEN: AtomicBool` 用 CAS 去重，满足「连点不叠加」。前端在对话框关闭（无论选了什么还是取消）后必须调用命令复位；同时在 `main` 的 `Destroyed` 与 `Focused(false)`? —— 不做额外兜底，改为**前端组件 unmount 时也调用复位**，并在后端的 `exit` / `hide` 动作执行前主动复位，避免任何一条路径留下卡死的 true。

## 5. 询问对话框契约

- 事件：`app://close-request`，载荷 `{ mode: "ask" | "confirm-terminals", runningTerminals: number }`。
  - **一个事件、一个组件、两种形态**，不开第二个事件。`ask` 形态下若 `runningTerminals > 0` 就在描述里内联一行终端警告，用户选「退出」即视为已确认；`confirm-terminals` 形态只有「取消 / 确认退出」，没有「记住我的选择」。两种形态共用同一个 `CLOSE_PROMPT_OPEN` 去重位，天然不会叠出两个框。
  - 确认框不新增命令：确认 = `resolve_close_request("exit", false)`，取消 = `resolve_close_request("cancel", false)`。
- 命令：

```rust
#[tauri::command]
pub async fn resolve_close_request(
    action: String,       // "minimize" | "exit" | "cancel"
    remember: bool,
    db: State<'_, AppDatabase>,
    app: AppHandle,
) -> Result<(), AppCommandError>
```

语义：
- `remember && action != "cancel"` → 先落库并刷新缓存，再执行动作。落库失败只记日志、不阻断动作——用户此刻的意图是关窗，不能因为写库失败卡住。
- `minimize` → `hide()` 主窗口；`exit` → `app.exit(0)`；`cancel` → 什么都不做。
- 三种情况都复位 `CLOSE_PROMPT_OPEN`。

- 命令：`get_system_close_behavior_settings` / `update_system_close_behavior_settings`，签名与 `get/update_system_rendering_settings` 对齐。

## 6. 前端

```
src/components/close-behavior/close-request-dialog.tsx
```

- `useEffect` 监听 `app://close-request`（走 `@/lib/tauri` 的事件封装，非 Tauri 环境直接不注册）。
- shadcn `AlertDialog`，标题「关闭 Codeg」，描述说明两种后果；`Checkbox` 记住我的选择；两个 action 按钮 + `AlertDialogCancel`。
- `onOpenChange(false)` 统一走 `resolve_close_request("cancel", false)`，覆盖 ESC / 点遮罩。
- 组件在主工作区根布局挂载一次，与既有全局对话框同级。

设置页：`general-settings.tsx` 中新增 `SettingRow`，`Select` 三选一。加载与保存复用该文件既有的 `loading` / `saving` / `loadError` 结构。托盘不可用时 `disabled` + 说明文案。

**平台判定**：`can_hide_to_tray()` 是后端状态（依赖托盘是否安装成功），前端无法自行推断。因此 `get_system_close_behavior_settings` 的返回体带上 `tray_available: bool`，让 UI 直接用它决定禁用与否，不要在前端用 `isLinux` 猜。

## 7. 双模式隔离

- 枚举与设置结构 cfg-gate 到 `tauri-runtime`（与 `SystemRenderingSettings` 一致）。
- 命令全部 `#[cfg(feature = "tauri-runtime")]`。
- 前端设置项外层用既有的桌面态判断（`general-settings.tsx` 里已有 `renderingSettingsLoadable` 这类模式判断的先例）包住，Web 模式不渲染。
- 验收要跑 `cargo check --no-default-features --bin codeg-server` 确认没有漏 gate。

## 8. 测试策略

Rust 单测（`#[cfg(all(test, feature = "tauri-runtime"))]`，与 `system_settings.rs` 既有测试模块同处）：
1. `close_behavior_defaults_to_ask` — 空库返回 `Ask`
2. `close_behavior_roundtrips` — 存 `exit` 后读回 `Exit`
3. `corrupt_close_behavior_falls_back_to_ask` — 库里塞 `"not json"` / `{"behavior":"boom"}` 均回落 `Ask` 且不 Err
4. `cache_reflects_update` — 更新后 `cached_close_behavior()` 同步变化
5. `count_live_by_owner_window` — 与 `kill_by_owner_window` 实际杀掉的数量一致（同一判活口径的回归）

前端单测（vitest）：
5. 对话框在收到事件后打开；`onOpenChange(false)` 触发 `cancel` 调用
6. 勾选记住 + 选退出 → 以 `("exit", true)` 调用命令
7. 事件重复触发时只渲染一个对话框

不可自动化的部分（真实关闭窗口、托盘召回、Linux 强制退出）在 PRD 验收清单里手工走。

## 9. 兼容与回滚

- 纯新增键，老版本升级上来读不到该键 → `Ask` → 首次关闭弹一次框。这正是期望的引导行为，不需要迁移。
- 回滚：删除该键即回到 `Ask`；代码回滚不留数据残骸（多一个无人读的 `app_metadata` 行，无害）。

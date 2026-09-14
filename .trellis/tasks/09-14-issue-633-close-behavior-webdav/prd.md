# 关闭行为选择与 WebDAV 自动备份 (issue #633)

## 需求来源

GitHub issue [xintaofei/codeg#633](https://github.com/xintaofei/codeg/issues/633)（作者 galact-byte）提出两个独立诉求：

1. **关闭按钮语义不透明**：点右上角关闭按钮实际只是隐藏到托盘，用户以为已经退出。希望能选择关闭行为。
2. **备份只能手动**：现有备份只有手动导出，容易忘记。希望有 WebDAV 自动同步。

用户额外要求：参考 [Stack-Cairn/LiveAgent](https://github.com/Stack-Cairn/LiveAgent) 的实现方式，但不照搬，要贴合 codeg 自身架构。

## 现状基线

| 面 | 现状 | 位置 |
|---|---|---|
| 关闭行为 | 硬编码：`can_hide_to_tray()` 为真则 `hide()`，否则 `exit(0)`。无任何用户可配置项，无首次提示 | `src-tauri/src/lib.rs:1149` |
| 托盘 | 已有托盘图标 + 「显示工作区」/「退出」菜单 | `src-tauri/src/commands/windows.rs` |
| 备份引擎 | 完整的加密 zip 归档引擎（快照 / 打包 / AES-256-GCM / 清单 / 恢复 / 回滚），已显式为 headless 调度预留 `EventEmitter::Noop` | `src-tauri/src/commands/backup/` |
| 备份 UI | 手动导出 + 手动恢复 | `src/components/settings/backup-settings.tsx` |
| 偏好持久化 | `app_metadata` 键值表 + `app_metadata_service::get_value/upsert_value`；系统级设置集中在 `system_settings.rs` | `src-tauri/src/commands/system_settings.rs` |
| 远端传输 | 无 WebDAV 能力 | — |

## 子任务划分

| 子任务 | 交付物 | 目录 |
|---|---|---|
| 主窗口关闭行为可配置 | 三值关闭偏好 + 首次询问对话框 + 设置页入口 | `.trellis/tasks/09-14-close-behavior-choice` |
| WebDAV 配置同步 | 配置快照采集/应用 + WebDAV 传输层 + 自动上传调度 + 设置页分区 | `.trellis/tasks/09-14-webdav-auto-backup` |

两个子任务无代码耦合，可独立实现、独立验收、独立提交。建议先做关闭行为（改动面小、能快速回应 issue 的主要痛点），再做 WebDAV。

### 关于第二个子任务的范围收敛

issue 原文说的是「备份」，但落地为**配置快照同步**而非全量归档上传。原因：codeg 的全量归档含数据库 + uploads + 外部 CLI 会话记录，可达 GB 级，定期整包推网盘会触发限流、吃满配额。参考 LiveAgent 的做法，只同步 codeg 自己拥有的配置数据（几十 KB 量级）。全量归档维持手动导出。UI 文案须明确写出「不含对话历史与上传文件」，避免用户误判。

## 跨子任务验收标准

- [ ] 两个子任务各自的验收标准全部通过
- [ ] `pnpm eslint .`、`pnpm test`、`pnpm build` 全绿
- [ ] `cargo clippy --all-targets --features test-utils -- -D warnings` 与 `cargo test --features test-utils` 全绿
- [ ] 服务器模式 `cargo check --no-default-features --bin codeg-server` 不因桌面专属代码而破坏
- [ ] 新增用户可见文案在 10 种语言的 `src/i18n/messages/*.json` 中均有条目，无缺键
- [ ] issue #633 的两条诉求在 UI 上均可被普通用户发现并使用

## 非目标

- 不做全量归档（数据库 / uploads / 会话记录）的自动上传
- 不做远端自动下载/自动恢复（静默覆盖本地数据的出错方向不可接受）
- 不做双向合并与冲突解决 UI
- 不改动现有备份归档格式与恢复流程

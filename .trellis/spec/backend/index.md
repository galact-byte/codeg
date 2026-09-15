# Backend Development Guidelines

> Code-specs for `src-tauri/` — executable contracts, not principles.

---

## Scope

This layer holds contracts that must hold across the desktop (`codeg`), server
(`codeg-server`), and companion (`codeg-mcp`) binaries. It is filled in
feature-by-feature: a file exists here only when a concrete contract was worth
freezing, so absence of a topic means "not written down yet", not "no rules".

---

## Guidelines Index

| Spec | Description | Status |
|------|-------------|--------|
| [Config Sync Contract](./config-sync-contract.md) | Config snapshot, WebDAV layout, command signatures, error matrix | Filled |

---

## Conventions That Apply To Every File Here

- **`_core` split**: business logic lives in `*_core` functions taking plain
  references (`&DatabaseConnection`, `&EventEmitter`). `#[tauri::command]`
  wrappers and Axum handlers are thin adapters over the same `_core` function,
  so a feature never gets two implementations.
- **Feature gating**: `#[cfg(feature = "tauri-runtime")]` guards desktop-only
  code. Anything a server build must reach may not sit behind that gate.
- **Errors**: `AppCommandError::invalid_input` for user-correctable input,
  `task_execution_failed` for internal failures. Attach a translation key when
  the frontend has to render the message.

---

**Language**: All documentation should be written in **English**.

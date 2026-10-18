# REFACTOR_PLAN — Project-S (safe cleanup)

Ветка: `refactor/safe-cleanup`. Дата: 2026-10-18.
Поведение, внешний вид и контракты — без изменений. Один срез = один концерн = один коммит.

## Текущее состояние
- Закрыто: **Фазы 0–2** (тесты, бэкенд-перф/безопасность, zero-risk фронтенд) и **S3.1–S3.3** (split `lib.rs`, split `harness_tools`, chat-run extract).
- Последний зелёный HEAD: `1dc0630` — `cargo test` **772 passed / 19 ignored**, `npm test` **82 passed**, build OK.
- Baseline-скриншоты: `/tmp/project-s-baseline/` (7 экранов).
- Полный `npm run app:build` — только в финале.

### S3.3 chat-run-extract — `done` (коммит `1dc0630`)
`chat/mod.rs` (2326 строк) → `mod.rs` (1332 — CRUD, история, message-UX, индексы) + `run.rs` (1088 — запуск: `start_project_run`, `spawn_run`, `run_agent_inner`, journal, подтверждения). Тела байт-в-байт идентичны HEAD (diff — только пустые строки и видимость): контракт `spawn_run`/`RunBridge`, порядок событий, thread-owned DB-соединение, persistence, confirmations — те же. Re-export'ы урезаны до нужного call sites: `start_project_run` (pub), `register_mcp_tools`/`RunBridge`/`spawn_run` (pub(crate)); `persist_final_answer` — `#[cfg(test)]` (зовёт только harness/notes в тестах); `ChannelConfirmer`/`ConfirmationRequestPayload`/`MAX_STEPS`/`temperature_for_kind` не ре-экспортируются (внешних call sites нет). Тесты: 17 mod-специфичных в `mod.rs`, 15 в `run.rs` (14 перенесены байт-в-байт + 1 новый `register_mcp_tools_hides_collisions_and_registers_the_rest_as_core` — отклонение: прерванная попытка добавила его с неверным ожиданием «lazy», исправлено на «core» по фактическому контракту `register(tool, true)`); общий `test_support` (`setup`/`call`) — `pub(crate)` в `mod.rs`. Проверки: `cargo test` 772 passed / 0 failed / 19 ignored (baseline 771/19, +1 новый тест), `cargo check` и `cargo clippy --all-targets` — 0 warnings (обе unused-re-export правки убраны), `npm run build` OK.

## Осталось (18 срезов)

### S3.4 loop-round-extract — `pending`
Разложить `run_inner` (`agent/loop.rs`) на именованные шаги; порядок событий/латч/guards — те же.

### S3.5 skills-split — `pending`
`skills/mod.rs` → `manifest` / `import` / `exec`; парсинг SKILL.md, конфликты импорта, script-exec — без изменений.

### S3.6 migration-table — `pending`
`run_migrations` → таблица миграций; SQL байт-в-байт, `user_version` и guard'ы v14/v17/v18 — те же.

## Фаза 4 — фронтенд-структура и перф
- **S4.1 chat-hook-dedupe** — `useChatRun` + `ChatComposer` вместо трёх копий; порядок событий/очередь те же.
- **S4.2 settings-tabs** — вкладка на компонент + `useDebouncedConfig`; save-on-blur тот же.
- **S4.3 harness-schedule-panel** — вынести панель расписаний из `HarnessView`.
- **S4.4 appstore-split** — Data/Actions контексты; `refresh()` только для списка проектов; замер ререндеров.
- **S4.5 memoize-chat** — `MessageRow`, стабильные колбэки, `BlockPreview`; DOM идентичен; замер стрима.
- **S4.6 topbar-event-driven** — убрать поллинг статуса; таймер только для отсчёта.
- **S4.7 search-hits-dedupe** — `useSearchHits`/`resolveEntry`; fallback тот же.
- **S4.8 modal-shell** — общий `Modal`; close-on-backdrop/Esc как сейчас.
- **S4.9 segmented** — общий контрол; классы `seg`/`active` те же.
- **S4.10 types-strict** — включить `strict`, типизировать конфиги; runtime не меняется.

## Фаза 5 — остаток
- **S5.1 dead-code-triage** — доказать и удалить/ограничить `allow(dead_code)`.
- **S5.2 trash-api** — `tasks.rs` через `reliability`.
- **S5.3 agent-util-dedupe** — `io_fail`, git-probe, envelope.
- **S5.4 module-cycle** — `ConfirmDecision` в лист.
- **S5.5 statics** — по одному process-static за срез (scheduler/reindex/browser/processes/backups/cache), только с тестами.
- **S5.6 today-snapshot** — `rejected`: добавление IPC-команды меняет публичный контракт.

## Проверка среза
`npm test`; `npm run build`; `npx oxlint src`; `cargo test`; `cargo check`; `cargo clippy --all-targets`; для перфа — замеры; для UI — скриншоты baseline; сверка `ipc.ts` и схемы БД; чистый `git status`.
Красное → откат, разбор, не «додавливать». Зелёное → `git add -A && git commit`, обновить этот файл.

## Риски
- Структурные срезы в god-файлах — только после зелёных тестов; контракты/DTO/схема/UI заморожены.
- `vitest`+`jsdom` уже добавлены (S0.2); новых зависимостей не добавляем.
- `BAG_REPORT.md` — приватный рабочий файл, не трогаем логику рефакторинга.

## Следующий шаг
**S3.4 loop-round-extract** — разложить `run_inner` (`agent/loop.rs`) на именованные шаги; порядок событий/латч/guards — те же.

# REFACTOR_PLAN — Project-S (safe cleanup)

Ветка: `refactor/safe-cleanup`. Дата: 2026-10-18.
Поведение, внешний вид и контракты — без изменений. Один срез = один концерн = один коммит.

## Текущее состояние
- Закрыто: **Фазы 0–2** (тесты, бэкенд-перф/безопасность, zero-risk фронтенд) и **S3.1–S3.5** (split `lib.rs`, split `harness_tools`, chat-run extract, loop-round extract, skills split).
- Последний зелёный HEAD: `<S3.5-hash>` — `cargo test` **775 passed / 19 ignored**, `npm test` **82 passed**, build OK.
- Baseline-скриншоты: `/tmp/project-s-baseline/` (7 экранов).
- Полный `npm run app:build` — только в финале.

### S3.3 chat-run-extract — `done` (коммит `1dc0630`)
`chat/mod.rs` (2326 строк) → `mod.rs` (1332 — CRUD, история, message-UX, индексы) + `run.rs` (1088 — запуск: `start_project_run`, `spawn_run`, `run_agent_inner`, journal, подтверждения). Тела байт-в-байт идентичны HEAD (diff — только пустые строки и видимость): контракт `spawn_run`/`RunBridge`, порядок событий, thread-owned DB-соединение, persistence, confirmations — те же. Re-export'ы урезаны до нужного call sites: `start_project_run` (pub), `register_mcp_tools`/`RunBridge`/`spawn_run` (pub(crate)); `persist_final_answer` — `#[cfg(test)]` (зовёт только harness/notes в тестах); `ChannelConfirmer`/`ConfirmationRequestPayload`/`MAX_STEPS`/`temperature_for_kind` не ре-экспортируются (внешних call sites нет). Тесты: 17 mod-специфичных в `mod.rs`, 15 в `run.rs` (14 перенесены байт-в-байт + 1 новый `register_mcp_tools_hides_collisions_and_registers_the_rest_as_core` — отклонение: прерванная попытка добавила его с неверным ожиданием «lazy», исправлено на «core» по фактическому контракту `register(tool, true)`); общий `test_support` (`setup`/`call`) — `pub(crate)` в `mod.rs`. Проверки: `cargo test` 772 passed / 0 failed / 19 ignored (baseline 771/19, +1 новый тест), `cargo check` и `cargo clippy --all-targets` — 0 warnings (обе unused-re-export правки убраны), `npm run build` OK.

## Осталось (17 срезов)

### S3.4 loop-round-extract — `done` (коммит `76f6d24`)
`run_inner` (`agent/loop.rs`, ~250 строк) разложен на именованные шаги в том же модуле: `Run` (per-run состояние: transcript/fallback/reasoning/allowlist/watchdog/`GuardState`) + `is_cancelled`, `check_watchdog`, `call_llm`, `note_round_outcome`, `step`, `handle_claim_guards`, `run_tool_call`, `note_tool_call`, `handle_loop_guard`, `finish_latch`, `finish_run`; колбэки `emit`/`on_message` — через приватный `Hooks`/`HookSet` (адаптер `exec_tool` к нетронутому `execute_tool_call`). Публичные `run`/`run_with`/`AgentRunConfig`/`AgentRunResult`/`AgentEvent`/`RunLimits`/`ChatBackend`/`AgentState` и `execute_tool_call`/`execute_inner`/`execute_with_timeout`/`attach_hint` — без изменений (grep diff: ни одной +/- строки в их сигнатурах). Порядок сохранён байт-за-байтом: cancel→watchdog→stream-события→trace/fallback-латч до аппенда→guards только после аппенда assistant-сообщения→внутри tool-call: Started→exec→Finished→links→tool-msg→bookkeeping→**latch→loop-nudge** (намеренно: finish_task не должен получать nudge — покрыто `guards_do_not_fire_when_finish_task_ends_the_run`). Тесты: 29 существующих loop-тестов без правок + 3 новых юнит-теста на извлечённые шаги (`finish_latch` defaults/не-finish/failed, `note_round_outcome` latch+blank-trace фильтры, `note_tool_call`/`handle_loop_guard` streak+verifying). Проверки: `cargo test` 775 passed / 0 failed / 19 ignored (baseline 772, +3), `cargo check` OK, `cargo clippy --all-targets` 0 warnings, `npm run build` OK.

### S3.5 skills-split — `done` (коммит `<S3.5-hash>`)
`skills/mod.rs` (2335 строк) → `manifest.rs` (фронтматтер-YAML-парсер, discovery builtin/user с conflicts/errors, enabled.json, нормализация имён и close-name подсказки, `body`/`load_full`, `skill`-тул, `list`/`set_enabled`) + `import.rs` (импорт folder/.zip/git со staging в Harness/tmp, установка и startup-синк builtin-набора, `create`, `create_skill`-тул) + `exec.rs` (`skill_exec`: resolve_entry/runner по exec-биту или расширению, cwd=папка скилла, TMPDIR→Harness/tmp, kill-timeout 120 s с clamp из `timeout_sec`, кап 30k на поток, argv без шелла) + `mod.rs` (re-export'ы + общий `#[cfg(test)] testutil` с `write_skill`/`scaffold`). Тела байт-в-байт идентичны HEAD (sorted-line diff против HEAD: только комментарии, per-file `use` и три расширения видимости `split_frontmatter`/`manifest_from_frontmatter`/`skill_info` → `pub(crate)`); правила парсинга, conflict/overwrite-семантика, runner-выбор, таймауты, капы и error envelopes — те же. Публичные пути call sites не менялись: `skills::{list,set_enabled,import,install_builtin,sync_builtin,create,SkillInfo,ImportResult,short_list,SkillTool,CreateSkillTool,SkillExecTool}`; re-export'ы урезаны до call sites (по образцу S3.3), `find`/`load_full` — `#[cfg(test)]` (зовут только тесты). Тесты: 33 skill-теста перенесены без правок по концернам — 11 в `manifest`, 11 в `import`, 11 в `exec`; общие хелперы — в `testutil`. Проверки: `cargo test` 775 passed / 0 failed / 19 ignored (baseline 775/19 — счёт тот же, тесты только перенесены), `cargo check --all-targets` и `cargo clippy --all-targets` — 0 warnings, `npm test` 82 passed, `npm run build` OK.

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
**S3.6 migration-table** — `run_migrations` → таблица миграций; SQL байт-в-байт, `user_version` и guard'ы v14/v17/v18 — те же.

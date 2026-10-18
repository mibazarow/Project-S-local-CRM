# REFACTOR_PLAN — Project-S (safe cleanup)

Ветка: `refactor/safe-cleanup`. База: `7e3a5c6` (WIP baseline). Дата: 2026-10-18.
Код не меняем до подтверждения плана. Каждый срез — один концерн, один коммит, полный прогон проверок.

## Цель и рамки
Поведение, внешний вид и контракты — без изменений. Снижаем сложность/дублирование, улучшаем поддерживаемость и производительность по замерам.
Не трогаем: 146 Tauri-команд и их сигнатуры, `src/api/types.ts` DTO, IPC-события, схему БД/миграции, `design/`, зависимости/lock (кроме отдельного согласования), бизнес-логику, тексты UI.

## Baseline (зафиксирован)
- `npm run build`: OK, 72 модуля, JS 529.51 kB (gzip 154 kB), **1.3 s**; `dist` 616K.
- `npx oxlint src`: **1 warning** (`crossActions.ts:16 no-control-regex`), 0 errors.
- `cargo test`: **729 passed, 0 failed, 16 ignored**, **10.1 s**.
- `cargo check`: 0 warnings; `cargo clippy --all-targets`: 0 warnings.
- `cargo build --release`: **31.6 s**; бинарник **56M**; `bundle` 180M.
- Запуск: dev-окно ~14 s; release — launch 0.05 s, окно ~3.9 s.
- SQLite (демо-данные): tasks_count 0.004 ms; `tasks_fts MATCH` 0.008 ms; `files_fts MATCH` 0.015 ms. FTS: tasks 60, files 85, messages 16.
- Данные: 5 проектов, 60 задач, 65 файлов проектов, 27 заметок.
- Скриншоты: `/tmp/project-s-baseline/*.png` (7 экранов; только окно приложения, демо-данные).

## Аудит — ключевые проблемы
### Бэкенд (P1)
1. `select_embedder()` создаёт новый `OrtEmbedder` на каждый поиск/индекс (`embed/mod.rs:44`), модель грузится заново → нарушает <300 ms.
2. Новое соединение SQLite на каждый поиск (`search/mod.rs:47`); `crm_search` — N соединений.
3. Индекс грузит все файлы/векторы проекта в память (`index/mod.rs:198,237,271`).
4. Три параллельных стека path-policy (`fs_ops`, `harness_tools`, `project_tools`) — риск расхождения.
5. `lib.rs` — 2546 строк, 146 команд, нет тестов.
6. `harness_tools.rs` — 3318 строк; `fs_transfer` 188 строк.
7. Нет characterization-тестов: `permissions.rs`, `watcher.rs`, `mcp/stdio.rs`, `index/walk.rs`, обёртки команд.
### Бэкенд (P2)
8. `lock().unwrap()` → отравление Mutex каскадом (`state.rs`, `lib.rs:60`).
9. DB-mutex держится во время файловых операций (`projects.rs:146`).
10. N+1 запросы при обогащении поиска (`search/mod.rs:154`).
11. `#![allow(dead_code)]` в `llm`/`skills`, item-level allow.
12. Дубли хелперов (`io_fail`, git-probe, envelope).
13. Дубли trash-SQL (`tasks.rs:616`).
14. `run_migrations` — 274-строчная цепочка if.
15. `run_inner` 250 строк (`loop.rs:248`).
16. `chat/mod.rs` смешивает CRUD + run + MCP + indexing.
17. `skills/mod.rs` — 2335 строк, 5 концернов.
18. `unwrap_or_default()` для `app.json` (`chat/mod.rs:1012`) — тихая потеря настроек.
19. Process-wide статики вместо AppState (scheduler/reindex/browser/processes/backups/cache).
20. Цикл `state ↔ agent` (`ConfirmDecision`).
21. SSH askpass-файлы не чистятся при краше.
### Фронтенд (P1)
1. Логика чата трижды скопирована (`ProjectChat`/`NotesChat`/`HarnessView`, ~600 строк).
2. `SettingsModal` — 2163 строки, ~45 `useState`, все вкладки грузятся сразу.
3. `AppStore` — единый контекст, `fs-changed` → `listProjects` + rerender всех.
4. `state.error` не отображается в основном шелле.
5. Нет мемоизации: каждая дельта стрима перерисовывает все сообщения + парсит Markdown.
6. `TopBar` поллит IPC каждые 1–5 с при наличии событий.
7. `toMessage` скопирован 6×; 8. даты/форматы — 3×; 15. debounce — 4×; 21. confirm-delete — 3×.
9. `HarnessView` — 1130 строк, ~25 состояний.
### Фронтенд (P2)
10. Мёртвые IPC-обёртки; 11. мёртвый CSS (`.memory-*`, `.mcp-list`, `.sub-head`); 12. hex вместо токенов; 13. дубли CSS-семейств; 14. таймеры queue-flush не чистятся; 16. дубли search-hits; 17. modal shell ×4; 18. нет `strict`, касты конфигов; 19. module-level счётчики; 20. Today N+1; 22. segmented ×3; 23. NoteEditor парсит Markdown в рендере; 24. гигиена; 25. нет фронтенд-тестов.

## Срезы (порядок = приоритет)
Статусы: `pending` → `in_progress` → `done` (коммит) / `rejected` (с причиной).

### Фаза 0 — Guardrails
- **S0.1 characterization-backend** — тесты на `permissions`, обёртки команд (in-memory DB), path-policy, scheduler tick, импорт/поиск. Prod-код не меняем. `done` (коммит `01cdf5b`) — добавлено 19 characterization-тестов (729→748 passed): матрица конфирмеров (`agent/permissions.rs`), матрица разрешения политик Skip/Check/CheckConditional c danger_reason (`agent/loop.rs`), обход walk (исключения, `.DS_Store`/БД, симлинки, rel-пути) (`index/walk.rs`), слой инструментов undo_last/undo_list — конверт, политики, события эмиттера (`agent/undo_tools.rs`), транспорт stdio — round-trip/таймаут/закрытый канал (`mcp/stdio.rs`), чистые хелперы watcher (`watcher.rs`). Уже покрыто ранее — пропущено: timeout→deny/autopilot/allow-always (chat-мост), лимиты/размер/символы/удаление строк (index/mod), обход zip/суффиксы коллизий (importer), кириллический FTS и скоупинг (search). Debounce watcher не тестирован: встроен в поток с `tauri::AppHandle`, требует изменения prod-кода.
- **S0.2 characterization-frontend** — golden-тесты чистых функций (`chatReducer`, `historyToItems`, `splitMarkdownBlocks`, `score`, `noteNameFromText`, `renderSnippet`). `done` (коммит `484eab7`) — добавлены `vitest`+`jsdom` (dev), `vitest.config.ts` (jsdom, `src/**/*.test.{ts,tsx}`), скрипт `npm test`; 65 characterization-тестов (5 файлов): редьюсер чата (delta/reasoningDelta/stepStart/stepFinish/done с reason→note/runError/queue+combineQueued/compaction, локальный vs удаленный run, монотонные ключи), `historyToItems` (роли, склейка assistant-групп, tool-steps и статусы по toolCallId, вложения с messageId/index, pinned, пустые группы), `splitMarkdownBlocks`/`joinMarkdownBlocks` (пустые строки, фенсы и правила закрытия, таблицы/списки, round-trip), `score` палитры (prefix/substring/fuzzy, капы 30/25, null; функция помечена `export` — только ключевое слово, логика не менялась), `noteNameFromText` (санитайз, 80 символов, `.md`), `renderSnippet` (FTS-маркеры → `<mark>`). Также покрыты чистые хелперы `toMessage`/`summarizeArgs`/`countMatches`. IPC замокан через `vi.mock("@tauri-apps/api/*")`. Пропущено: `src/shared/...` — такого каталога нет (чистых хелперов вне перечисленного не найдено; `src/hooks/usePointerDrag` — хук, не чистая функция). Проверки: `npm test` 65/65; `npm run build` OK (JS 529.51 kB — как baseline, тесты не бандлятся); `npx oxlint src` — 1 warning как в baseline, 0 errors.

### Фаза 1 — Безопасность/производительность (бэкенд, замеры)
- **S1.1 embed-cached-instance** — кэш `OrtEmbedder` на процесс; тесты векторов идентичны; бенч поиска до/после. `done` (коммит `080079f`) — процессный кэш `Arc<dyn Embedder>` в `embed/mod.rs` (`OnceLock<Mutex<HashMap<PathBuf, Arc<dyn Embedder>>>>`, ключ — каталог модели): `select_embedder` отдаёт тот же инстанс всем search/index-вызовам, ONNX-сессия/токенizer грузятся один раз; Noop при отсутствующей модели не кэшируется (дешев, чтобы модель, появившаяся между вызовами, подхватывалась). Черты `Embedder`/`backend_id`/`dim`/`embed_status` и векторы не менялись. Тесты: счётчик-фабрика (1 конструкция на ключ, `Arc::ptr_eq`, идентичные векторы), переиспользование инстанса в `select_embedder`, `#[ignore]` тайминг на реальной модели. Бенч (реал. модель, release): вызов «как раньше» (новый инстанс) = 664 мс загрузка+эмбед; после кэша: 2-й `select_embedder` = 31 мкс, тёплый эмбед = 39 мс (dev: 2773 → 39 мс). `live_model` детерминизм подтверждён (sim 0.8493 > 0.6225). Проверки: `cargo test` 750 passed / 17 ignored; `cargo check`, `cargo clippy --all-targets` — 0 warnings; `npm run build` OK.
- **S1.2 search-conn-reuse** — одно соединение на поиск; `crm_search` без N соединений; тесты hits/порядка. `done` (коммит `29ad758`) — `search_with` стал `pub(crate)`, все legs (FTS/semantic/task-FTS/task-semantic/mtime/snippet) уже принимали `&Connection` — `db::open` внутри legs не вызывается; `crm_search` (`harness_tools.rs`) открывает ОДНО соединение на весь вызов (резолв проекта + все проекты) вместо `search_project` на каждый проект (было N соединений); `search_notes`/`search_chats`/`global_search` — по одному соединению на вызов (как было). Тесты: thread-local счётчик `db::open_counter` (`db/mod.rs`, cfg(test)) — `search_with` = 0 открытий на всех legs, `crm_search` = ровно 1 открытие (cross-project и scoped); хиты/скоры/порядок/scoping/лимиты не менялись (`crm_projects_search_tasks_and_datas_round_trip` проходит без правок). Бенч (демо-БД, release, 3 прогона): гибрид по 5 проектам «по соединению на проект» = 1188.6/1100.0/1123.8 мс, «одно соединение» = 1097.8/1102.8/1113.6 мс (полное время доминирует ONNX-эмбед, экономия ~0.8 мс = 4×`db::open`); FTS-only путь (noop): 1.57/1.55/1.53 → 0.43/0.43/0.42 мс (~3.6×). Проверки: `cargo test` 752 passed / 18 ignored; `cargo check`, `cargo clippy --all-targets` — 0 warnings; `npm run build` OK.
- **S1.3 search-batch-enrich** — один `IN (...)` вместо N запросов; сниппеты/скоры идентичны. `done` (коммит `259793f`) — обогащение хитов в `search_with` (по запросу `file_mtime`+`file_content` на хит, N+1) заменено на `enrich_hits`: один `IN (...)`-запрос mtime по `indexed_files` и один по `files_fts` (content — только для хитов с пустым сниппетом, как раньше) на чанк из 500 хитов; task-хиты пропускаются (как и раньше: для пустого `rel_path` строки нет → `modified = 0`, свой сниппет); фолбэки для отсутствующих строк идентичны (mtime 0, пустой content); `files_fts.project_id` — как был TEXT. Чанкинг — страховка от лимита переменных SQLite. Контракты/схема/DTO не менялись; добавлена фича `trace` rusqlite (только для тестового счётчика запросов, новых зависимостей нет, `Cargo.lock` не менялся). Тесты: `enrichment_carries_per_hit_mtime_and_fallback_snippet` (разные mtime на хит, FTS-сниппет сохранён, fallback-сниппет из content для semantic-only хита, mtime 0 для удалённой строки), `enrichment_uses_batched_queries_instead_of_one_per_hit` (trace утверждений: ровно 2 батч-запроса на вызов с 6 хитами, похитовых запросов нет); `#[ignore]` тайминг на демо-БД: полный FTS-only вызов по 5 проектам (30 хитов, запрос `example`) до = 1.27/0.89/0.87 мс (mean 1.01), после = 0.93–1.52 мс (разница ниже уровня шума на демо-масштабе); стадия обогащения изолированно: per-hit 0.117/0.096/0.098 → batched 0.057/0.048/0.059 мс (~2×); digest всех значений хитов (rel|mtime|snippet, FNV-1a) идентичен до/после = `5e884f8c718303c3`. Проверки: `cargo test` 754 passed / 19 ignored; `cargo check`, `cargo clippy --all-targets` — 0 warnings; `npm run build` OK; `npx oxlint src` — 1 warning как в baseline.
- **S1.4 poison-tolerant-accessors** — восстановление после отравления Mutex; тест с паникой. `done` (коммит `0f44cdb`) — новый модуль `sync_util.rs`: `lock_recover(&Mutex<T>)`, `read_recover`/`write_recover` (RwLock-эквиваленты) через `unwrap_or_else(|e| e.into_inner())` — игнорируют флаг отравления и отдают guard; данные под этими локами либо заменяются целиком (`Option<Connection>`, статусы), либо это SQLite-соединение под WAL + busy_timeout. Заменены все `.lock().unwrap()` на долгоживущем состоянии: `AppState` (db/working_dir/watcher/mcp/journal/emitter — `state.rs`, `lib.rs` вкл. `with_db`, `get_working_dir`, setup/exit), `AgentRegistry` (pending/allow_always/cancels), scheduler-поток (`scheduler/mod.rs:674,677`), reindex-статус (`search_reindex.rs` — 6 мест + working_dir в таймере), `processes::CHILDREN`, `journal::current`, `open_with::handler_cache`, `watcher` (watcher/db), emitter-хелперы (`chat`, `memory`, `notes`, `todos`, `objectives`, `harness`, `agent/{project_tools,send_file,harness_tools,undo_tools}`), chat-run поток (thread_state/persist_state/journal). Тестовые локальные мьютексы (моки-бэкенды, рекордеры) не тронуты; уже толерантные места (`browser_tool`, `ops::ACTIVE_BACKUPS`) не менялись. Поведение неотравленных локов идентично, контракты/команды/IPC без изменений. Тесты: `sync_util` — отравление Mutex и RwLock паникующей нитью, хелперы отдают рабочий guard; `lib::tests::with_db_recovers_after_the_db_mutex_is_poisoned` — temp-БД, паника под DB-локом, `with_db` продолжает работать. Проверки: `cargo test` 757 passed / 19 ignored (+3 новых); `cargo check`, `cargo clippy --all-targets` — 0 warnings; `npm run build` OK.
- **S1.5 short-db-transactions** — файловые операции вне DB-мьютекса; rollback rename сохранён.
- **S1.6 path-policy-unify** — единый модуль; три вызова; корпус тестов accept/reject идентичен.
- **S1.7 askpass-startup-purge** — чистка старых askpass-каталогов при старте; тест.
- **S1.8 config-error-surface** — ошибка `app.json` не теряется; валидный путь идентичен.

### Фаза 2 — Zero-risk фронтенд-свипы
- **S2.1 shared-errors** — один `toMessage` + шимы; текст fallback идентичен.
- **S2.2 shared-format** — даты/форматы; строки ru-RU идентичны.
- **S2.3 use-debounced** — 4 копии → хук; задержки те же.
- **S2.4 confirm-remove** — один helper; строки те же.
- **S2.5 dead-ipc-wrappers** — удалить неиспользуемые обёртки (Rust-команды остаются).
- **S2.6 dead-css** — удалить 13 селекторов; визуальный diff = 0.
- **S2.7 css-tokens** — `--toolbar`, `--danger-soft`, `--danger-tint`; computed styles идентичны.
- **S2.8 hygiene** — форматирование/мёртвые экспорты; без визуальных изменений.

### Фаза 3 — Структурные разбиения (бэкенд)
- **S3.1 commands-split** — `lib.rs` → `commands/{projects,tasks,chat,fs,settings,...}`; `generate_handler!` байт-в-байт.
- **S3.2 harness-tools-split** — `fs_tools`/`crm_tools`/`path_policy`; `fs_transfer` → plan/execute/undo.
- **S3.3 chat-run-extract** — `chat/run.rs`; контракт `spawn_run` тот же.
- **S3.4 loop-round-extract** — шаги `run_inner`; порядок событий/латч тот же.
- **S3.5 skills-split** — `manifest/import/exec`; парсинг/конфликты те же.
- **S3.6 migration-table** — таблица миграций; SQL байт-в-байт, `user_version` тот же.

### Фаза 4 — Фронтенд-структура и перф
- **S4.1 chat-hook-dedupe** — `useChatRun` + `ChatComposer`; порядок событий/очередь те же.
- **S4.2 settings-tabs** — компонент на вкладку + `useDebouncedConfig`; save-on-blur тот же.
- **S4.3 harness-schedule-panel** — вынести панель расписаний.
- **S4.4 appstore-split** — Data/Actions контексты; refresh только для списка проектов; замер rerender.
- **S4.5 memoize-chat** — `MessageRow`, стабильные колбэки, `BlockPreview`; HTML идентичен; замер.
- **S4.6 topbar-event-driven** — убрать поллинг статуса; таймер только для отсчёта.
- **S4.7 search-hits-dedupe** — `useSearchHits`/`resolveEntry`; fallback тот же.
- **S4.8 modal-shell** — общий Modal; close-on-backdrop/Esc как сейчас.
- **S4.9 segmented** — общий контрол; классы `seg`/`active` те же.
- **S4.10 types-strict** — включить `strict`, типизировать конфиги; runtime не меняется.

### Фаза 5 — Остаток
- **S5.1 dead-code-triage** — доказать и удалить/ограничить allow.
- **S5.2 trash-api** — `tasks.rs` через `reliability`.
- **S5.3 agent-util-dedupe** — `io_fail`, git-probe, envelope.
- **S5.4 module-cycle** — `ConfirmDecision` в лист.
- **S5.5 statics** — по одному статику за срез (scheduler/reindex/browser/processes/backups/cache) — только с тестами.
- **S5.6 today-snapshot** — НЕ делаем без согласования: добавление IPC-команды = изменение контракта.

## Проверка каждого среза
`npm run build`; `npx oxlint src`; `cargo test`; `cargo check`; `cargo clippy --all-targets`; для перф-срезов — повторные замеры; для UI — сравнение скриншотов baseline (`/tmp/project-s-baseline`); сверка `src/api/ipc.ts` и схемы БД; `git status` чист.
Красное → откат среза (git revert/сброс рабочего дерева), разбор, не «додавливать».
Зелёное → `git add -A && git commit` с осмысленным сообщением; обновить этот файл (статус, коммит, следующий шаг).

## Риски и допущения
- Риск регрессий в god-файлах: сначала characterization-тесты, затем структурные срезы.
- Контракты/схема/UI заморожены; любое изменение — стоп и согласование.
- `BAG_REPORT.md` отслеживается git и может содержать реальные SSH-реквизиты (репозиторий приватный). Историю не переписываем; новые коммиты файл не трогают. Решить: оставить / убрать из индекса без rewrite.
- Фронтенд-тесты: `vitest`+`jsdom` согласованы и добавлены в dev-зависимости (S0.2).
- Эталонные скриншоты — на демо-данных, вне репозитория.
- Полный `npm run app:build` — только на финальном этапе.

## Следующий шаг
**S1.5 short-db-transactions** — файловые операции вне DB-мьютекса; rollback rename сохранён.

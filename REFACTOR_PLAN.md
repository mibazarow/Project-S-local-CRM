# REFACTOR_PLAN — Project-S (safe cleanup)

Ветка: `refactor/safe-cleanup`. Дата: 2026-10-18.
Поведение, внешний вид и контракты — без изменений. Один срез = один концерн = один коммит.

> **Правило ведения:** выполненный срез **удаляем из этого файла полностью**. Никаких `done`, зачёркиваний и списков «закрыто». В файле — только актуальный остаток. Закрыл срез → удалил абзац.

## Текущее состояние
- Последний зелёный HEAD: `356a3d8` — `npm test` **92 passed**, `npm run build` OK, oxlint — только pre-existing `crossActions.ts`.
- Бэкенд последний раз проверен на `f23b488`: `cargo test` **777 passed / 19 ignored**, `cargo check`/`clippy --all-targets` 0 warnings. S4.1–S4.2 бэкенд не трогали.
- Baseline-скриншоты: `/tmp/project-s-baseline/` (7 экранов).
- Полный `npm run app:build` — только в финале.

## Фаза 4 — фронтенд-структура и перф

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
Красное → откат, разбор, не «додавливать». Зелёное → `git add -A && git commit`, обновить этот файл (удалить закрытый срез).

## Риски
- Структурные срезы в god-файлах — только после зелёных тестов; контракты/DTO/схема/UI заморожены.
- `vitest`+`jsdom` уже добавлены (S0.2); новых зависимостей не добавляем.
- `BAG_REPORT.md`/`Q.md` — рабочие файлы задач, не менять их в рамках рефакторинга.

## Следующий шаг
**S4.4 appstore-split** — Data/Actions контексты; `refresh()` только для списка проектов; замер ререндеров.

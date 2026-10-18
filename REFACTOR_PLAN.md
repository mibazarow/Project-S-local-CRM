# REFACTOR_PLAN — Project-S (safe cleanup)

Ветка: `refactor/safe-cleanup`. Дата: 2026-10-18.
Поведение, внешний вид и контракты — без изменений. Один срез = один концерн = один коммит.

> **Правило ведения:** выполненный срез **удаляем из этого файла полностью**. Никаких `done`, зачёркиваний и списков «закрыто». В файле — только актуальный остаток. Закрыл срез → удалил абзац.

## Текущее состояние
- Последний зелёный HEAD: `ab60b04` — `npm test` **164 passed**, `npm run build` OK, oxlint — только pre-existing `crossActions.ts`.
- Бэкенд последний раз проверен на `10ae06d`: `cargo test` **777 passed / 19 ignored**, `cargo check`/`clippy --all-targets` 0 warnings. Контракты/DTO/схема не затронуты.
- Baseline-скриншоты: `/tmp/project-s-baseline/` (7 экранов).
- Полный `npm run app:build` — только в финале.

## Фаза 5 — остаток
- **S5.2 trash-api** — `tasks.rs` через `reliability`.
- **S5.3 agent-util-dedupe** — `io_fail`, git-probe, envelope.
- **S5.4 module-cycle** — `ConfirmDecision` в лист.
- **S5.5 statics** — по одному process-static за срез (scheduler/reindex/browser/processes/backups/cache), только с тестами.

## Проверка среза
`npm test`; `npm run build`; `npx oxlint src`; `cargo test`; `cargo check`; `cargo clippy --all-targets`; для перфа — замеры; для UI — скриншоты baseline; сверка `ipc.ts` и схемы БД; чистый `git status`.
Красное → откат, разбор, не «додавливать». Зелёное → `git add -A && git commit`, обновить этот файл (удалить закрытый срез).

## Риски
- Структурные срезы в god-файлах — только после зелёных тестов; контракты/DTO/схема/UI заморожены.
- `vitest`+`jsdom` уже добавлены; новых зависимостей не добавляем.
- `BAG_REPORT.md`/`Q.md` — рабочие файлы задач, не менять их в рамках рефакторинга.

## Следующий шаг
**S5.2 trash-api** — `tasks.rs` через `reliability`.

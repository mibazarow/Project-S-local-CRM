# REFACTOR_PLAN — Project-S (safe cleanup)

Ветка: `refactor/safe-cleanup`. Дата: 2026-10-18.
Поведение, внешний вид и контракты — без изменений. Один срез = один концерн = один коммит.

> **Правило ведения:** выполненный срез **удаляем из этого файла полностью**. Никаких `done`, зачёркиваний и списков «закрыто». В файле — только актуальный остаток. Закрыл срез → удалил абзац.

## Текущее состояние
- Последний зелёный HEAD: `ab60b04` — `npm test` **164 passed**, `npm run build` OK, oxlint — только pre-existing `crossActions.ts`.
- Бэкенд последний раз проверен на `3537e88`: `cargo test` **791 passed / 19 ignored**, `cargo check`/`clippy --all-targets` 0 warnings. Контракты/DTO/схема не затронуты.
- Baseline-скриншоты: `/tmp/project-s-baseline/` (7 экранов).
- Полный `npm run app:build` — только в финале.

## Фаза 5 — остаток
- **S5.5b reindex-static** — `GLOBAL` + `TIMER_STARTED` (`search_reindex.rs:208/214`): инкапсулировать менеджер и one-shot таймер.
- **S5.5c browser-static** — `SESSION` (`agent/browser_tool.rs:34`): инкапсулировать общую сессию (launch/reuse/kill).
- **S5.5d processes-static** — `CHILDREN` (`processes.rs:40`): инкапсулировать реестр процессов (shutdown/reconcile).
- **S5.5e backups-static** — `ACTIVE_BACKUPS` (`ops.rs:260`): инкапсулировать дедуп конкурентных бэкапов.
- **S5.5f open-with-cache** — `CACHE` (`open_with.rs:153`): инкапсулировать кэш списка приложений по типу.
- `embed`-кэши (`EMBEDDER_CACHE`, ORT `ENV`) — осознанно процессные, не трогаем.

## Проверка среза
`npm test`; `npm run build`; `npx oxlint src`; `cargo test`; `cargo check`; `cargo clippy --all-targets`; для перфа — замеры; для UI — скриншоты baseline; сверка `ipc.ts` и схемы БД; чистый `git status`.
Красное → откат, разбор, не «додавливать». Зелёное → `git add -A && git commit`, обновить этот файл (удалить закрытый срез).

## Риски
- Структурные срезы в god-файлах — только после зелёных тестов; контракты/DTO/схема/UI заморожены.
- `vitest`+`jsdom` уже добавлены; новых зависимостей не добавляем.
- `BAG_REPORT.md`/`Q.md` — рабочие файлы задач, не менять их в рамках рефакторинга.

## Следующий шаг
**S5.5b reindex-static** — инкапсулировать `GLOBAL` + `TIMER_STARTED` (`search_reindex.rs`) и one-shot таймер, покрыть тестом повторный вызов.

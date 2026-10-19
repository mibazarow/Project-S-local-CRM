# REFACTOR_PLAN — Project-S (safe cleanup)

Ветка: `refactor/safe-cleanup`. Дата: 2026-10-18.
Поведение, внешний вид и контракты — без изменений. Один срез = один концерн = один коммит.

> **Правило ведения:** выполненный срез **удаляем из этого файла полностью**. Никаких `done`, зачёркиваний и списков «закрыто». В файле — только актуальный остаток. Закрыл срез → удалил абзац.

## Текущее состояние
- Финальный зелёный HEAD: `c11586b` — `npm test` **299 passed**, `npm run build` OK, oxlint — только pre-existing `crossActions.ts`; `cargo test` **899 passed / 19 ignored**, `cargo check`/`clippy --all-targets` 0 warnings. Контракты/DTO/схема/UI не затронуты.
- Финальный `npm run app:build` — OK; установка в `/Applications/Project-S.app` (`npm run app:update`); установленный бандл несёт новую иконку (`Contents/Resources/icon.icns` побайтово = `src-tauri/icons/icon.icns`, `CFBundleIconFile=icon.icns`).
- Автосмоук установленного приложения: окно рендерит дефолтный вид «Проекты», ранее активный проект восстанавливается (persist `project-s:lastProject:*` в WKWebView localStorage), процесс жив, выход по SIGTERM без crash-репорта. Логи чистые: 0 panic/backtrace/unwrap, 0 error/fault в unified log за прогон (совпадения `error` — только в `runs-2026-10-17.jsonl`, до прогона).
- Ограничение TCC: интерактивный клик-обход вкладок невозможен (у CLI-хоста нет Accessibility). Активный вид/вкладка **не персистятся** (`tab` = `useState("projects")`, `App.tsx:19`; FilesPanel/Settings — тоже `useState`), поэтому Today / Задачи / Блокнот / Харнес / Настройки покрыты **164 фронтенд-тестами** (`TopBar`, `FilesPanel`, `TodayView`, `HarnessView`, `NotesView`, `SettingsModal`, `Modal`), а не ручным обходом.

## Проверка среза
`npm test`; `npm run build`; `npx oxlint src`; `cargo test`; `cargo check`; `cargo clippy --all-targets`; для перфа — замеры; для UI — скриншоты baseline; сверка `ipc.ts` и схемы БД; чистый `git status`.
Красное → откат, разбор, не «додавливать». Зелёное → `git add -A && git commit`, обновить этот файл (удалить закрытый срез).

## Риски
- Структурные срезы в god-файлах — только после зелёных тестов; контракты/DTO/схема/UI заморожены.
- `vitest`+`jsdom` уже добавлены; новых зависимостей не добавляем.
- `BAG_REPORT.md`/`Q.md` — рабочие файлы задач, не менять их в рамках рефакторинга.

## Следующий шаг
Очередь пуста. Следующая работа — `Q.md`.

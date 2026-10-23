# Project-S — техническая документация

Этот файл — техническое продолжение [`README.md`](../README.md): архитектура, данные, агентский луп, поиск, провайдеры, сборка, тесты. Для нетехнического чтения предназначен README.

Текущий статус: **v1.0.5**, схема БД **v40**, **38** встроенных скиллов, **1092 passed / 19 ignored** бэкенд-тестов (Rust) и **567** фронтенд-тестов (vitest). macOS (Apple Silicon) и Windows 11 x64 — **опубликованы** в [релизе 1.0.5](https://gitverse.ru/mibazarow/Project-S-local-CRM/releases/tag/v1.0.5); Linux — в плане (Flatpak).

## Что это

Project-S — локальное десктопное приложение «один пользователь, одна машина»: CRM по проектам, файлы, задачи (канбан/список), заметки, AI-чат с контекстом проекта и универсальный агент «Харнес» с доступом к компьютеру. Серверной части нет: весь код, данные, индексы и ключи лежат на машине пользователя. LLM подключается напрямую к любому OpenAI-совместимому API (или локальной модели).

## Архитектура

```
React UI  ──Tauri IPC──  Rust core  ──  SQLite / файлы проекта
                              │
                              ├── агентный луп (tool calls → ответ)
                              ├── LLM-клиент (стриминг, провайдеры)
                              ├── поиск (FTS5 + эмбеддинги ONNX)
                              └── харнес (файлы, shell, браузер, SSH, MCP)
```

- **Оболочка** — Tauri 2: одно нативное окно, WebView системы (WKWebView на macOS, WebView2 на Windows).
- **Бэкенд** — Rust: агентный луп, SQLite (данные + FTS5 + векторы), ONNX Runtime для эмбеддингов (без Python), CDP-управление браузером, планировщик, MCP-клиент.
- **Фронтенд** — React 19 + TypeScript + Vite, vanilla CSS (без Tailwind), UI на русском, светлая тема в стиле macOS.
- **Платформенный шов** — `src-tauri/src/platform/`: платформенно-нейтральные обёртки (home dir, processes, permissions, console); платформенная специфика — в `platform/`, `tauri.windows.conf.json` и `scripts/windows/`, а не в бизнес-логике.

## Данные и layout

Рабочая директория по умолчанию — `~/Documents/PROJECT-S` (выбирается в онбординге):

```
PROJECT-S/
├── Projects/    # проекты; одна папка = один проект (файлы пользователя)
├── Harness/     # рабочая территория харнеса
└── Settings/    # config (llm.json и др.), skills, .db, logs, backups, models
```

- **SQLite** (`Settings/.db`) — метаданные проектов, задачи, чаты, заметки, расписания, память агента, FTS5-индексы, векторы эмбеддингов. Миграции версионируются, текущая схема — **v40**.
- **Файлы проектов** — обычные папки на диске: их видно в Finder/Проводнике, их можно бэкапить как обычно. Приложение — не «чёрный ящик», а работа поверх нормальных директорий.
- **Конфиги dev и release изолированы**: `~/Library/Application Support/app.project-s.ru/` (release) и `…/dev/` (dev), активная директория — в `working_dir.txt`; переменная `PROJECT_S_CONFIG_DIR` задаёт папку конфига явно.

## Агентский луп, инструменты, разрешения

AI-чат проекта и Харнес работают через один агентский луп: стриминг ответа модели, вызовы инструментов (tool calling), сбор контекста проекта, подтверждения опасных действий.

**Инструменты** (реализованы в `src-tauri/src/agent/`):

| Группа | Примеры инструментов |
|---|---|
| Файлы | `fs_list/read/write/copy/move/rename/grep/watch`, `read_file`, `write_file`, `edit_file`, `apply_patch`, `trash_paths`, `undo_last` |
| CRM/задачи | `crm_search`, `crm_tasks`, `task_create/update/move/delete`, `project_set_archived` |
| Заметки | `notes_list/read/create/write/move/rename/copy` |
| Поиск | `search` (FTS5 + семантика), `fs_grep`, `mdfind`, `search_chats`, `search_notes` |
| Интернет | `websearch`, `webfetch`, `http_request`, `browser` (CDP: навигация, клики, скриншоты) |
| Терминал/процессы | `bash`/`shell` (PowerShell/cmd на Windows), `process_start/stop/list/logs` |
| SSH | `ssh_exec`, `ssh_generate_key`, `ssh_install_key`, профили в `ssh_list_profiles` |
| Git | `git_status/diff/log/commit/pull/push/branch` |
| Документы | `create_document`, `create_spreadsheet`, `pdf_split/merge/fill_form/to_image`, `ocr`, `vision`, `image_transform`, `screenshot`, `macos_ui`, `clipboard_*` |
| Планировщик | `schedule_create/list/pause/delete` — cron-запуски агента по расписанию |
| Память/навыки | память агента, 38 встроенных скиллов (markdown-инструкции), `mcp_add` (MCP-серверы) |
| Настройки | `settings_list/propose/apply` — агент предлагает изменения конфига, пользователь подтверждает |

**Разрешения.** Опасные действия (удаление, запись вне рабочей области, shell, сеть, SSH) требуют подтверждения в UI. Файловые операции ограничены path-policy: рабочая директория + явные согласия. Удаления идут в корзину, есть `undo_last`.

## Харнес

Универсальный агент вне привязки к проекту: сессии с историей, собственные расписания, память, скиллы, MCP-серверы, рабочая папка `Harness/`. Видит контекст проектов (CRM, задачи, файлы, заметки) и может выполнять длинные многошаговые задачи: от «сделай HTML-игру и открой» до «проверь сайт по расписанию и доложи».

## Поиск и индексация

- **SQLite FTS5** — быстрый полнотекстовый поиск по файлам, задачам, чатам, заметкам.
- **Локальные эмбеддинги** — модель `embeddinggemma-2` (~300 МБ, квантованный ONNX) через ONNX Runtime, без Python и облака; векторы хранятся в той же SQLite. Гибрид FTS + семантика.
- Модель **не лежит в git и не вшита в установщик** (с 1.0.3): приложение скачивает её разово после онбординга с Hugging Face `onnx-community/embeddinggemma-2-ONNX` в `Settings/models/embeddinggemma-2/` — с уведомлением в онбординге, прогресс-баннером и повтором при ошибке (кнопка и в Настройки → Агент). Без модели приложение работает, но поиск деградирует до FTS. Для разработчиков остался ручной стейдж: `npm run model:stage` (`scripts/stage-embed-model.sh`, `scripts/windows/stage-embed-model.ps1`).

## Провайдеры LLM

- Неограниченное число профилей провайдеров; активен ровно один, переключение в настройках.
- Любой OpenAI-совместимый endpoint (`/chat/completions` + tool calling): DeepSeek, Qwen, локальные серверы (llama.cpp/Ollama-совместимые), прочее.
- Ключ и endpoint активного провайдера хранятся в локальном `Settings/config/llm.json` (в открытом виде — осознанный trade-off локального приложения; файл не покидает машину).
- Шаблоны провайдеров, проверка подключения и загрузка списка моделей при сохранении.

## Безопасность и локальность

- Нет облачного бэкенда, телеметрии и аккаунтов: приложение не отправляет данные проектов на свои серверы.
- Данные, ключи, история чатов и индексы — на машине пользователя; бэкап = копирование папки.
- Подтверждения опасных операций, path-policy, корзина/undo, изоляция dev/release конфигов.
- В демо-данных и скриншотах — только вымышленные клиенты и TEST-NET-адреса; реальные данные в репозиторий не попадают.

## Платформы и сборки

| Платформа | Статус v1.0.5 | Упаковка |
|---|---|---|
| macOS (Apple Silicon) | **опубликовано:** [v1.0.5](https://gitverse.ru/mibazarow/Project-S-local-CRM/releases/tag/v1.0.5) | DMG `Project-S-1.0.5-macOS-ARM.dmg` (21 804 979 Б, SHA256 `4a28bd40…a6eb`); нотаризация ещё не сделана |
| Windows 11 x64 | **опубликовано:** [v1.0.5](https://gitverse.ru/mibazarow/Project-S-local-CRM/releases/tag/v1.0.5) | ZIP `Project-S-1.0.5-Windows-x64-Setup.zip` (14 276 267 Б, SHA256 `8a468f85…ebf`), внутри NSIS `Project-S-1.0.5-Windows-x64-Setup.exe` (14 292 714 Б, SHA256 `f0265cd8…f4c`) |
| Linux | в плане | Flatpak (один пакет для большинства дистрибутивов) |

- Релизный тег `v1.0.5` — на `master`; macOS и Windows собраны из одного коммита. Публичный релиз: https://gitverse.ru/mibazarow/Project-S-local-CRM/releases/tag/v1.0.5. Linux будет добавлен в следующий тег.
- Windows-сборка идёт с macOS по SSH на build-хост; production-hardening (подпись, updater, Credential Manager) для текущей Windows-сборки не блокирует. Подробности — [`CROSPLATFORM.md`](../CROSPLATFORM.md), [`DOCS/WINDOWS-PORT.md`](WINDOWS-PORT.md).
- Linux: WebKitGTK, Flatpak, XDG, sandbox, Wayland — [`DOCS/LINUX-PORT.md`](LINUX-PORT.md).
- На Windows нужны `onnxruntime.dll` и MSVC runtime; CRT-конфликт решён вендорингом `esaxx-rs` (`vendor/`, `[patch.crates-io]`).

## Сборка из исходников

Нужны Node.js 20+, Rust stable и Xcode Command Line Tools (macOS) или VS Build Tools 2022 + Rust MSVC (Windows).

```bash
npm install
npm run tauri dev     # приложение в dev-режиме
```

```bash
npm test              # фронтенд-тесты (vitest) — 561
cargo test            # бэкенд-тесты (из src-tauri) — 1092 passed / 19 ignored
npm run build         # tsc + vite build
npm run app:build     # релизная сборка Tauri (macOS)
npm run app:install   # сборка + установка в /Applications/Project-S.app
npm run app:update    # установить уже собранный бандл
```

Локальный контроль качества:

```bash
scripts/check.sh            # fmt → clippy → cargo test → npm test → tsc → vite build
STRICT_FMT=1 scripts/check.sh
```

Один проход всех проверок с остановкой на первом падении. `cargo fmt --check` сейчас показывает счётчик и не валит gate (репозиторий форматируют руками; `STRICT_FMT=1` делает шаг обязательным). `clippy -D warnings` идёт со списком `-A` на уже существующие долги — новые линты роняют gate.

После клонирования в git нет `node_modules`, `src-tauri/target` и локальной копии модели эмбеддингов — она не нужна для сборки: с 1.0.3 приложение скачивает модель само после онбординга. Скрипты стейджа (`npm run model:stage`) — только для разработчиков, работающих с моделью локально.

## Layout репозитория

```
src/                  # React UI: components/{projects,tasks,files,notes,chat,harness,settings,onboarding}
src-tauri/src/        # Rust: agent/, chat/, harness/, db/, search/, index/, embed/, llm/,
                      # skills/, scheduler/, mcp/, memory/, projects/, notes/, cdp/, ssh/, platform/
src-tauri/resources/  # skills/builtin (38), models/ (не в git)
scripts/              # check.sh, install-app.sh, stage-embed-model.sh, windows/
design/               # статический HTML/CSS/JS-прототип UI на моках
DOCS/                 # документация: about.md, Q.md, REFACTOR_PLAN.md, порты ОС
```

## Известные ограничения

- Windows: production-hardening (подпись, updater) — в плане; GUI-проверки выполняет пользователь.
- Linux — реализации пока нет.
- Ключи LLM в `llm.json` хранятся в открытом виде (локально); системный keychain — в плане.
- Однопользовательский режим; синхронизации между машинами нет (перенос = копирование рабочей папки).
- Модель эмбеддингов (~300 МБ) скачивается приложением разово после онбординга; при отсутствии сети поиск остаётся только по словам (FTS).

## Лицензия и исходники

Project-S распространяется по **source-available** лицензии, а не по лицензии с открытым исходным кодом (OSI). Официальные сборки бесплатны для любых целей, включая коммерческие. Модификация приложения и перепродажа/лицензирование приложения или его изменённых версий требуют предварительного письменного разрешения автора. Исходники открыты для просмотра в публичном репозитории; изменять, публиковать и распространять их без разрешения нельзя. Авторство `Project-S by Михаил Базаров (bazarow.ru)` обязательно. Полный текст — [`LICENSE.md`](../LICENSE.md), правила безопасности — [`SECURITY.md`](../SECURITY.md), вклад — [`CONTRIBUTING.md`](../CONTRIBUTING.md).

## Дорожная карта

- Релизы на GitVerse: macOS DMG и Windows NSIS — [1.0.5 опубликованы](https://gitverse.ru/mibazarow/Project-S-local-CRM/releases/tag/v1.0.5), Linux Flatpak — следом.
- Linux-порт (Flatpak) — [`DOCS/LINUX-PORT.md`](LINUX-PORT.md).
- Windows: подпись, updater, паритет инструментов — [`DOCS/WINDOWS-PORT.md`](WINDOWS-PORT.md).
- Живые очереди: [`DOCS/Q.md`](Q.md) (задачи/баги), [`DOCS/REFACTOR_PLAN.md`](REFACTOR_PLAN.md) (срезы рефакторинга).
- Протокол и статус кроссплатформенной работы: [`CROSPLATFORM.md`](../CROSPLATFORM.md).

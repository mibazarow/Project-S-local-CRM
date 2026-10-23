# context.md — Project S: передача в новую сессию

Дата: 2026-10-19; обновлено: 2026-10-21 (добавлен §12 — Windows-порт и окружение). Этот файл — краткий, но полный снимок состояния проекта, решений и договорённостей. Прочитай его целиком перед первым действием.

## 1. Что за проект

**Project S** — локальное десктопное приложение: CRM + AI-агент + агентский харнес. Один пользователь, всё на своей машине, без облака и серверной части. LLM подключается через OpenAI-совместимый API.

Коротко: папка проекта — это и есть проект. Внутри — файлы, задачи, доступы, заметки и чат с агентом, который видит контекст проекта. Отдельно «Харнес» — универсальный агент с доступом к компьютеру: файлы, терминал, браузер, SSH, планировщик, память, скиллы, MCP.

- Репозиторий: `~/Projects/Project-S`
- Рабочая директория приложения по умолчанию: `~/Documents/PROJECT-S`
- Установленное приложение: `/Applications/Project-S.app`
- Ветка: `master`
- Стек: Tauri 2 + Rust (свой агент-луп), React 19 + TypeScript + Vite, vanilla CSS (без Tailwind), SQLite (rusqlite, FTS5, эмбеддинги).
- UI только на русском; светлая macOS-тема, акцент `#376A9F`, тёмный тулбар `#182533`, сегментные контролы.
- `design/` — статический HTML/CSS/JS-прототип на моках, не читает и не пишет диск.

## 2. Что уже сделано

- Рефакторинг S0–S5 завершён. `DOCS/REFACTOR_PLAN.md` пуст.
- Все 27 пунктов из `DOCS/Q.md` + новая иконка реализованы, проверены, закоммичены. `DOCS/Q.md` пуст.
- Документация перенесена в `DOCS/`. В корне остаются `README.md`, `AGENTS.md` и этот `context.md`.
- `SPEC.md` удалён по решению пользователя. Отдельного ТЗ нет: требования живут в `DOCS/Q.md` и `DOCS/BAG_REPORT.md`.
- `README.md` переписан под текущее состояние; `AGENTS.md` обновлён под `DOCS/`.
- Созданы планы портирования:
  - `DOCS/WINDOWS-PORT.md` — только Windows 11, WebView2 + fallback, установщик, пути, подпись, CI.
  - `DOCS/LINUX-PORT.md` — WebKitGTK, Flatpak как основной канал, AppImage, XDG, sandbox, Wayland.
- Версия приложения поднята до `1.0.0`:
  - `package.json`, `package-lock.json`
  - `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock` (`project-s`)
  - `src-tauri/tauri.conf.json`
- Собрано и установлено:
  - `npm run app:build` — успешно, собраны `.app` и `Project-S_1.0.0_aarch64.dmg`.
  - `npm run app:update` — установлено в `/Applications/Project-S.app`.
  - Проверено: `CFBundleShortVersionString = 1.0.0`, размер ~59 MB.
- Последние полные прогоны (до финальной сборки, код с тех пор не менялся):
  - `cargo test`: 899 passed, 19 ignored.
  - `npm test`: 299 passed.
  - `npm run build`: OK.
  - oxlint: только старое предупреждение в `crossActions.ts`.
- Kilo-конфиг почищен:
  - удалён провайдер/агент Ornith;
  - удалён файл `~/.config/kilo/agents/ornith-coder.md`;
  - `bxmcp` (Bitrix MCP) выключен для Kilo; в PhpStorm он живёт отдельно;
  - добавлен лёгкий субагент `builder` (сейчас на `deepseek/deepseek-flash`; им выполнена финальная сборка/установка — см. §7).

## 3. Что сейчас является «остатком»

Активной очереди нет: `DOCS/Q.md` и `DOCS/REFACTOR_PLAN.md` пусты.

Тема **субагентов и контекста** закрыта:

- В свежей сессии `general` на Qwen (240k) всё равно упал с `Compaction exhausted: context still exceeds model limits after 3 attempts` — новая сессия не помогла.
- Поэтому `agent.general.model` и `agent.builder.model` переведены на `deepseek/deepseek-flash` и проверены: smoke-тест `general` на `DOCS/` прошёл, `builder` проверил `git status` и версию установленного приложения.
- `explore`, `build`, `plan` остаются на Qwen.

Опционально, не блокирует:

- В коде местами остались комментарии-метки вида `(SPEC …)`, хотя `SPEC.md` удалён. Можно вычистить отдельным маленьким срезом, если пользователь попросит.
- Windows: demo-сборка работает, остаток — в `DOCS/WINDOWS-PORT.md`. Linux — только план.

## 4. Важные решения и почему так

- **Нет отдельного ТЗ.** Пользователь отказался от `SPEC.md`; требования фиксируются по ходу в `DOCS/Q.md`, сырые заметки — в `DOCS/BAG_REPORT.md`.
- **Документация только в `DOCS/`.** В корне — `README.md`, `AGENTS.md`, плюс этот handoff-файл по прямой просьбе.
- **`Q.md` и `REFACTOR_PLAN.md` — живые очереди.** Выполненный пункт удаляется полностью, без «done», зачёркиваний и пометок. История — в git.
- **Windows — только Windows 11.** Windows 10 не поддерживаем.
- **Flutter-миграции нет.** Пользователь её отменил; остаются Tauri + Rust + React.
- **Сборки ставим только в `/Applications/Project-S.app`.** Не на Рабочий стол.
- **Прототип `design/` не трогает диск.** Только захардкоженные моки в `app.js`.
- **Версия 1.0.3** — текущий релизный номер; установленная программа показывает 1.0.2 до следующей установки сборки.

## 5. Правила работы (обязательно)

- Одновременно — **не более одного субагента**. Последовательно: один закончил → следующий.
- Одна задача субагенту = **один концерн**. Дробить крупную работу на атомарные проверяемые срезы.
- Промпты субагентам и их ответы — **на английском**. Пользователю пересказывать **на русском**, кратко.
- После каждого отработавшего субагента/среза — **коммит всего рабочего дерева**: `git add -A && git commit …`. Не выбирать файлы по одному.
- Перед коммитом проверять сборку/тесты, если менялся код.
- Не добавлять комментарии в код без просьбы.
- Нет-код и исследовательские задачи — в `explore`, а не в основной контекст.
- Для команд/сборки/установки — `builder` (DeepSeek), для полноценных правок — `general` (Qwen, если влезает) или DeepSeek.
- Использовать скилл `ponytail` на нетривиальных кодовых задачах: минимальное полное решение, без оверинжиниринга.
- UI-тексты и имена, которые видит пользователь, — русские; код/идентификаторы — английские.
- CSS — обычные классы, без фреймворков; держаться macOS-эстетики.

## 6. Безопасность данных (важно, стрим)

- Экраны приложения показываются в стриме. **Нельзя** класть в моки, скриншоты, документацию и примеры реальные данные: домены клиентов, ФИО, логины, пароли, FTP/IP, номера договоров, пути к рабочим папкам пользователя.
- Все примеры — вымышленные: `example.ru`, `ИП Петров`, IP из TEST-NET `192.0.2.0/24`.
- Реальные пользовательские директории (например рабочие папки клиентов) не читать вслух, не переименовывать, не удалять без явной просьбы.
- Перед показом/скриншотом — grep на реальные названия клиентов и доступы.
- API-ключи и токены не попадают в репозиторий и документацию.

## 7. Как устроен Kilo в этом проекте

Глобальный конфиг: `~/.config/kilo/kilo.jsonc`.

- Основная модель (оркестратор): `deepseek/deepseek-flash` (1M).
- `agent.general`: `deepseek/deepseek-flash`, `permission.mcp: deny`.
- `agent.explore`: `llm-server/Qwen-Flash-Vision`.
- `agent.build`: `llm-server/Qwen-Flash-Vision`.
- `agent.plan`: `llm-server/Qwen-Flash-Vision`.
- `agent.builder`: `deepseek/deepseek-flash`; кастомный лёгкий субагент: read/edit/bash, без MCP, веба и вложенных задач.
- `agent.compaction`: `deepseek/deepseek-flash`.
- `compaction`: auto, `threshold_percent: 85`, `reserved: 30000`, `prune: true`, `tail_turns: 2`.
- `subagent_model`: `llm-server/Qwen-Flash-Vision`.
- MCP: `bxmcp` — `enabled: false`; `rea` и `scrapling` — включены.
- `ornith`-провайдер удалён; `ornith-coder.md` удалён.

Решение пользователя: **`explore`, `build`, `plan` — на Qwen; `general`, `builder`, оркестратор и модель сжатия — на DeepSeek.**

Почему `general` падал (вопрос закрыт):

- 240k — это не «запас на всё»: в контекст входят системный промпт, схемы инструментов, MCP и унаследованная история сессии.
- `explore` с урезанным набором инструментов влезал, `general` с полным — нет; свежая сессия этого не изменила.
- `builder` на Qwen тоже падал, на DeepSeek с 1M — сразу заработал. Значит, дело в размере контекста, а не в MCP или коде.
- Компакт и перезапуск не заменяют новую сессию: история остаётся. Итог: `general` и `builder` переведены на DeepSeek.

## 8. Ключевые файлы

- `README.md` — обзор проекта, функции, стек, быстрый старт.
- `AGENTS.md` — правила работы агентов, субагенты, документы, безопасность.
- `DOCS/Q.md` — живая очередь задач/багов (сейчас пуста).
- `DOCS/REFACTOR_PLAN.md` — живая очередь рефакторинга (сейчас пуста).
- `DOCS/BAG_REPORT.md` — сырые заметки пользователя после тестирования.
- `DOCS/WINDOWS-PORT.md` — план Windows 11.
- `DOCS/LINUX-PORT.md` — план Linux.
- `design/` — статический прототип UI.
- `src-tauri/` — Rust-бэкенд, агентный луп, SQLite, поиск, харнес.
- `src/` — React/TS-фронтенд.
- `scripts/install-app.sh` — установка собранного бандла в `/Applications`.

## 9. Команды

```bash
cd ~/Projects/Project-S

npm test                 # фронтенд-тесты
cd src-tauri && cargo test && cd ..
npm run build            # tsc + vite
npm run app:build        # релизная сборка Tauri
npm run app:update       # установить уже собранный бандл
npm run app:install      # сборка + установка

defaults read /Applications/Project-S.app/Contents/Info CFBundleShortVersionString
```

Прототип:

```bash
python3 -m http.server 8731 --directory design
# Playwright-снапшоты → design/preview/, после проверки сервер остановить
```

Git:

```bash
git status --short
git log -5 --oneline
git add -A && git commit -m "…"
```

## 10. Что делать в новой сессии (порядок)

1. Прочитать этот файл и `AGENTS.md`.
2. Проверить, что дерево чистое: `git status --short`.
3. Проверить установленную версию: `defaults read … CFBundleShortVersionString` → `1.0.2`.
4. Проверка `general` на Qwen уже выполнена: в свежей сессии `general` упал с `Compaction exhausted…`, поэтому `agent.general.model` и `agent.builder.model` переведены на `deepseek/deepseek-flash` и проверены. Повторять не нужно.
5. Не переделывать завершённый рефакторинг и не восстанавливать `SPEC.md`.
6. Дальше — только по новой очереди от пользователя: сначала фиксировать пункт в `DOCS/Q.md`, потом реализация, тесты, коммит, удаление пункта.

## 11. Чего не делать

- Не включать `bxmcp` в Kilo без необходимости: он тяжёлый, а в PhpStorm уже настроен отдельно.
- Не запускать тяжёлые write-субагенты на Qwen 240k в большой сессии.
- Не считать перезапуск Kilo очисткой контекста.
- Не переносить документацию обратно в корень.
- Не добавлять реальные данные клиентов в моки, скриншоты и docs.
- Не менять версию и не собирать релиз без явной просьбы.
- Не трогать `/Applications/Project-S.app` вручную; только через `npm run app:update` / `app:install`.

## 12. Windows-порт: анализ и состояние окружения (2026-10-21)

> Снимок от 2026-10-21. Актуальный статус Windows — `CROSPLATFORM.md` §5 и `DOCS/WINDOWS-PORT.md`; ниже перечислены состояния, которые с тех пор закрыты.

Прочитан `DOCS/WINDOWS-PORT.md` (план от 2026-10-21, v1.0.2), сверен с кодом и `CROSPLATFORM.md`. Вопросы Windows закрыты macOS-агентом: решения зафиксированы в `CROSPLATFORM.md` §0.1, временные записи журнала удалены.

Окружение Windows 11 x64 подготовлено:

- Node.js 24.22.1 / npm 11.19.1; PowerShell execution policy `CurrentUser → RemoteSigned`.
- Rust 1.99.1 `stable-x86_64-pc-windows-msvc` + `rustfmt`, `clippy`.
- Visual Studio Build Tools 2022 + `VCTools` + Windows SDK; WebView2 155.0.4283.62.
- Python 3.13.15; Git Bash в PATH; `npm ci` выполнен.
- Модель `embeddinggemma-2` доставлена в `src-tauri/resources/models/embeddinggemma-2/` (4 файла, ~346 МБ; gitignore).
- `npm run build` — OK; `npm test` — 532/532; `npx tauri info` — среда зелёная.
- `cargo check` — 11 ошибок P0 в 4 файлах: `processes.rs`, `skills/exec.rs`, `cdp/mod.rs`, `agent/ssh_tools.rs`.

Суть плана:

- Паритет 1:1 с macOS; только Windows 11 24H2/25H2, x64 MSVC.
- P0-1…P0-7: cfg-разделение Unix-кода, Job Objects + kill tree, PowerShell/cmd вместо `sh`, `dirs` вместо `HOME`, обёртка над `canonicalize` (`\\?\`), ресурсы `exe_dir/resources/skills`, `tauri.windows.conf.json` + `bundle.windows`.
- Этапы 0–6; оценка 10–14 недель + буфер = 13–19 календарных недель.
- 18 открытых вопросов: WebView2 (fixedRuntime/fallback), NSIS/MSI, ARM64, секреты (plaintext/DPAPI), доставка модели и ONNX DLL, updater, подпись, Store/MSIX, OCR, OpenSSH, Recycle Bin и др.

Решения macOS (`CROSPLATFORM.md` §0.1):

1. Платформенный шов: `platform/mod.rs` + `platform/macos` появляются в `master` до merge Windows-кода. В бизнес-логике никаких `#[cfg(unix)]`; временный cfg допустим только как scaffold в `port/windows`, не в `master`.
2. Shell: платформенно-нейтральный инструмент `shell` (`auto`/PowerShell/cmd/sh), `bash` — алиас на Unix. Windows реализует PowerShell/cmd; скиллы не форкаются, жёсткие bash-примеры правит macOS в `master`.
3. UI/UX: решения за macOS. На Windows нативный titlebar, Segoe UI, Ctrl вместо Cmd, DPI через платформенный слой; без редизайна.
4. WebView2: baseline **109.0.1518.78** (Tauri 2); ниже или отсутствует — fixed runtime. Перед релизом сверить с release notes Tauri.
5. Ресурсы: единый `resource_dir()` + fallback `exe_dir/resources` для dev. Helper добавляет macOS; `paths.rs` — `notes_dir` и `harness_inbox_dir`. `package.json` — кросс-платформенный `scripts/stage-embed-model.mjs`, bash-обёртка остаётся.

Уточнения по коду:

- §0.4 не упоминает `Notes/` и `Harness/Входящие` (`paths.rs`).
- `package.json` жёстко вызывает `bash scripts/stage-embed-model.sh` — на Windows первый build упадёт на npm-шаге.
- Скелета `platform/`, `tauri.<os>.conf.json` и `scripts/windows/` в репозитории нет — объём P0-7 больше, чем в плане.

Следующий шаг:

1. Дождаться в `master` платформенного шва `platform/mod.rs` + `platform/macos` и helper'а ресурсов от macOS; до этого допустим только scaffold в `port/windows`.
2. Ветка `port/windows` от `master`.
3. `scripts/windows/stage-embed-model.ps1` / `.mjs`, убрать зависимость npm-скриптов от `bash`.
4. `tauri.windows.conf.json` + `bundle.windows`.
5. P0-1…P0-7, первый end-to-end build и smoke.

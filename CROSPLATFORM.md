# CROSPLATFORM.md — протокол связи macOS ↔ Windows ↔ Linux

> ## ОБЯЗАТЕЛЬНО ДЛЯ ВСЕХ АГЕНТОВ
>
> 1. **Перед любой работой** прочитай этот файл целиком и `AGENTS.md`.
> 2. **После каждого изменения**, которое влияет на общий код, контракты, БД, пути, конфиги, сборку или релиз, добавь запись в «Журнал» ниже: что сделано, какой коммит, что это значит для другой платформы и что ей нужно сделать.
> 3. **Не удаляй чужие открытые вопросы и записи.** Закрытые Q&A после подтверждения удаляются: если вопрос решён и понят — удали свой вопрос и ответ на него; важное решение оставь одной строкой в §0.1 «Решения». История — в git.
> 4. **Секреты, API-ключи, реальные клиентские данные и личные пути в этот файл не писать.** Только вымышленные примеры.
> 5. **Перед работой — `git pull` / merge `master`.** После работы — коммит и push. Другая сторона увидит изменения только после pull.
> 6. **Одна долгоживущая ветка — `master`.** Ветки `macos`/`windows`/`linux` не создаём. Платформенная специфика живёт в `platform/` и конфигах сборки, а не в отдельной ветке.
> 7. Если меняешь контракт платформенного слоя (трейт, сигнатуру, формат события) — сначала изменение в `master`, запись в этом файле, потом реализация на другой платформе.
> 8. **macOS — единственная площадка разработки продукта.** Все новые фичи, изменения UX/дизайна, бизнес-логики, схемы БД и текстов рождаются на macOS и попадают в `master`. Windows/Linux только применяют эти изменения, пишут платформенный слой и собирают. Новых продуктовых решений на Windows/Linux нет.
> 9. **Каждое обновление этого файла — сразу `git add CROSPLATFORM.md && git commit && git push`.** Перед работой — `git pull`. Пользователь не является прокси: агенты общаются через файл и remote, а не через пересылку сообщений.
> 10. **CROSPLATFORM.md — живой файл, не архив.** В нём только текущее состояние, открытые вопросы/actions и краткие решения. Закрытые Q&A после подтверждения удаляются; важные выводы — одной строкой в §0.1 «Решения».
> 11. **Режим по умолчанию — macOS.** Windows-сборка/порт запускаются **только по явной команде пользователя** («а теперь делаем для Windows»). Не собирать Windows автоматически после каждого изменения и не начинать Windows-работу самостоятельно.
> 12. **На Windows нет агента.** Windows — только build/test-хост: `git pull`, сборка, тесты, артефакты. Весь код, включая `platform/`, пишет macOS-агент; на Windows код не правится и не коммитится.

Это не чат, а **журнал решений и статуса**, который ведёт macOS-агент, плюс handoff для сборки на Windows. Windows-агента нет; пользователь только просит собрать и проверяет GUI.

---

## 0. Главное правило: разработка только на macOS

- **macOS-агент — единственный, кто делает продукт**: интерфейс, функционал, бизнес-логику, схему БД, миграции, тексты, дизайн, агентские сценарии.
- **Windows- и Linux-агенты — только порт и сборка**: `platform/<os>`, WebView2/NSIS/Flatpak, системные API, упаковка, платформенные тесты. Они не придумывают фичи и не меняют поведение продукта.
- Любое изменение продукта сначала появляется в `master` на macOS. Windows/Linux делают `pull`/merge `master` и применяют его у себя.
- Если на Windows/Linux что-то не работает из-за отсутствия платформенной реализации — это **порт**, его можно делать на месте. Если нужно изменить общее поведение, UI, схему БД или бизнес-логику — сначала запись в этом журнале `[ACTION: macos]`, решение и реализация на macOS, потом порт.
- Платформенный фикс не должен менять общий контракт «втихую». Меняешь контракт — сначала `master` + запись здесь.

## 0.1. Решения (кратко, не удалять)

1. **Платформенный шов:** `platform/mod.rs` + `platform/macos` — в `master` до merge Windows-кода. Никаких `#[cfg(unix)]` в бизнес-логике; временный cfg допустим только как scaffold в `port/windows`, не в `master`.
2. **Shell:** платформенно-нейтральный `shell` (`auto`/PowerShell/cmd/sh); `bash` — алиас на Unix. Windows реализует PowerShell/cmd. Скиллы не форкаем; жёсткие bash-примеры правит macOS в `master`.
3. **UI/UX:** решения за macOS. Windows — нативный titlebar, Segoe UI, Ctrl вместо Cmd, DPI через платформенный слой; без редизайна.
4. **WebView2:** baseline **109.0.1518.78** (Tauri 2); по решению пользователя используем онлайн-загрузчик `downloadBootstrapper`, fixed runtime не поставляем.
5. **Ресурсы:** единый `resource_dir()` + fallback `exe_dir/resources` для dev. macOS добавляет helper; `paths.rs` — `notes_dir` и `harness_inbox_dir`. С 1.0.3 модель эмбеддингов **не бандлится**: приложение скачивает её само после онбординга в `<workingDir>/Settings/models/embeddinggemma-2/` (HF `onnx-community/embeddinggemma-2-ONNX`, ~300 МБ, UI-прогресс, FTS-fallback). Скрипты `scripts/stage-embed-model.sh` / `scripts/windows/stage-embed-model.ps1` — только для разработчиков (`npm run model:stage`).
6. **Vendor CRT-патч:** `esaxx-rs 0.1.10` вендорен в `vendor/esaxx-rs` и подключён через `[patch.crates-io]` в `src-tauri/Cargo.toml`; в `build.rs` для Windows убран `static_crt(true)` — иначе LNK2038/LNK2005 против dynamic CRT (`/MD`) у prebuilt ONNX Runtime. Не возвращать. Дельта против crates.io только эта: плюс удалённые секции `[[example]]`/`[[bench]]` (их каталоги не вендорены). Патч нужен, пока `tokenizers` → `esaxx-rs` в дереве (`ort-backend` — дефолтный бэкенд эмбеддингов).

## 0.2. Режим работы: macOS по умолчанию, Windows по команде

- **Обычный режим:** работаем только на macOS — фичи, полировка, дизайн, функционал, тесты, релиз macOS.
- **Windows-режим:** включается **только явной командой пользователя** («а теперь делаем для Windows»). Никаких автоматических Windows-сборок на каждый push или после каждой macOS-правки.
- **Windows — рабочая сборка для локального использования.** Production-hardening (подпись, updater, миграция данных, Credential Manager, Recycle Bin и полный паритет инструментов) ведётся отдельно; GUI-проверки выполняет пользователь.
- **По команде:** macOS-агент подключается к Windows по SSH, делает `pull`/merge `master`, применяет изменения, собирает, гоняет тесты, забирает артефакты и пишет отчёт. Пользователь проверяет GUI на Windows.
- **Пакетно:** накапливаем правки на macOS и собираем Windows **по команде**, а не на каждый мелкий фикс. Перед сборкой — пакет изменений, тесты macOS, затем одна Windows-сборка.
- **Windows-сессия сама не начинает работу**: ждёт команду пользователя или `[ACTION: windows]` в этом журнале.
- CI на каждый push для Windows не обязателен; сборка — on-demand, пока не решим иначе.

## 1. Роли и ветки

| Роль | Где | Ветка | Что делает |
|---|---|---|---|
| macOS-агент | Mac | `master` + короткие `feat/*`, `fix/*` | **Единственный разработчик**: продукт, `platform/`, сборка, релизы macOS |
| Windows build host | Windows | — | **Нет агента.** Только `git pull`, сборка, тесты, артефакты. Код не правится и не коммитится |
| Linux build host (позже) | Linux | — | **Нет агента.** Только сборка |

> **Сборка Windows — с macOS по SSH (основной путь).** Windows-хост — это build/test target: macOS-агент подключается по SSH, делает `pull`, сборку, тесты и забирает артефакты. Windows-агента нет; пользователь только просит собрать и проверяет GUI.

Правила:

- `master` — единый источник правды и защищённая ветка. Все изменения — через PR/merge.
- Постоянных веток `macos`/`windows`/`linux` и временной `port/windows` **нет**. Весь код, включая `platform/`, пишет macOS-агент в `feat/*`/`fix/*` или `master`.
- Windows build host не коммитит: только `git pull`, сборка, тесты, артефакты. Все коммиты и push делает macOS.
- Релизные теги (`v1.0.6`) ставятся на `master`; артефакты собираются под каждую ОС из одного коммита.
- Если Windows временно отстаёт — используем pre-release-тег (`v1.0.3-win.1`), а не отдельную версию кода.

## 2. Код: платформенный слой, а не форк

```
src-tauri/src/platform/
  mod.rs            # швы: home/рабочая папка, is_executable, restrict_permissions,
                    # hide_console, reveal_in_file_manager — различия ОС через cfg
  process.rs        # управление процессами: background_command, kill_alive,
                    # signal_group, liveness_target — различия ОС через cfg

src-tauri/tauri.windows.conf.json          # есть
scripts/windows/stage-embed-model.ps1      # есть
```

Каталогов `platform/macos|windows|linux`, `scripts/macos|linux` и файлов
`tauri.macos|linux.conf.json` нет: платформенные различия лежат cfg-ветками в
двух файлах шва, отдельные каталоги заведём только когда кода станет больше.

- Общая бизнес-логика (задачи, чаты, БД, агент, скиллы, память, поиск) — **одна** для всех ОС.
- `platform/` пишет macOS-агент; Windows-хост только компилирует, тестирует и отдаёт артефакты. Кода на Windows нет.
- Платформенные вызовы — только через `platform/`. Не размазывать `#[cfg(target_os)]` по бизнес-логике.
- Cargo-зависимости — через `[target.'cfg(target_os = "...")'.dependencies]`.
- Упаковка, WebView2, fixed runtime, подпись, installer — в `tauri.windows.conf.json` / `scripts/windows/`.
- Фронтенд: отличия (Cmd/Ctrl, шрифты, titlebar, DPI, zoom) — в одном `platform`-модуле и CSS-переменных, без форка UI.
- Схема БД и миграции — общие. Не делать платформенные миграции и отдельные БД-ветки.
- Общие контракты (трейты, события, IPC-структуры) считаются **замороженными**; изменение — отдельный PR в `master` + запись здесь.

## 3. Синхронизация

Ежедневный/ежесменный цикл:

1. `git switch master && git pull`.
2. Прочитать этот файл и `AGENTS.md`.
3. Работать в своей ветке на macOS: продукт и весь код, включая `platform/`.
4. Обновить «Журнал»: что сделано, коммит, влияние, что проверить на Windows.
5. Прогнать тесты/сборку, коммит, push. Windows-сборка — только по команде, с macOS по SSH (см. §3.1).
6. Windows build host делает `git pull` для сборки; код не правит и не коммитит.
7. Пользователь проверяет GUI на Windows; macOS-агент забирает артефакты и фиксирует результат в журнале.

Если возник конфликт в этом файле — **оставить обе записи**, не удалять чужую. Конфликты в коде решать в `platform/<os>` и конфигах; общую логику не раздваивать.

## 3.1. Сборка Windows с macOS по SSH (основной путь)

До появления CI Windows собираем **с Mac по SSH**, а не вручную через пользователя.

Что должно быть на Windows:
- OpenSSH Server включён, служба `sshd` запускается автоматически, порт 22 разрешён в firewall;
- отдельный пользователь для сборок, вход по **публичному ключу**, пароль по возможности отключён;
- доступ через Tailscale/ZeroTier/WireGuard, не открывать SSH в интернет;
- Git, Node.js/npm, Rust MSVC, VS Build Tools 2022, WebView2;
- репозиторий склонирован, `git pull` работает, модель `embeddinggemma-2` доставлена в `src-tauri/resources/models/`;
- известен путь репозитория на Windows.

На Mac:
- alias в `~/.ssh/config` (например, `Host win11`), ключ, без пароля в репозитории;
- проверка: `ssh win11 'hostname; whoami'`;
- на Windows достаточно read-only deploy key или доступа на `git pull`; коммитов и push с Windows нет;
- доступы, ключ, fingerprint и предусловия — в `DOCS/WINDOWS-ACCESS.md`.

Сборка:

```bash
ssh win 'powershell -NoProfile -ExecutionPolicy Bypass -File C:\path\Project-S\scripts\windows\stage-embed-model.ps1'
ssh win 'powershell -NoProfile -Command "cd C:\path\Project-S; git pull; npm ci; npx tauri build --config src-tauri/tauri.windows.conf.json"'
```

Конфиг упаковки обязателен: без `--config src-tauri/tauri.windows.conf.json` Tauri
берёт macOS-конфиг (dmg, нет NSIS/WebView2). `npm run app:build` и `app:install`
на Windows не использовать — они вызывают bash (`scripts/stage-embed-model.sh`,
`scripts/install-app.sh`).

Забрать артефакты:

```bash
scp -r "win:C:/path/Project-S/src-tauri/target/release/bundle" ./artifacts/windows/
```

Правила:
- сборка может идти headless; GUI-тесты и первый запуск — на Windows/RDP;
- после сборки проверять `.msi`/`.exe`, checksums и лог; артефакты копировать на Mac;
- если SSH недоступен — временно CI, но не ручная пересылка файлов;
- в CROSPLATFORM не писать ключи, пароли и приватные пути; только alias/имя пользователя/статус;
- на Windows нет агента и правок кода: только сборка/тесты; все коммиты и push делает macOS; пользователь проверяет GUI.

## 4. Definition of Done для кроссплатформенного изменения

- [ ] Общая логика — в `master`, платформенная — в `platform/<os>`.
- [ ] Для Windows/Linux: изменение уже существует в `master`; это платформенная адаптация, а не новая фича.
- [ ] Windows-check не сломан: `cargo check --target x86_64-pc-windows-msvc` (или Windows CI).
- [ ] Тесты/сборка зелёные на затронутой платформе.
- [ ] Нет секретов, реальных клиентских данных и личных путей в новых файлах.
- [ ] Запись в «Журнале» с коммитом и action item для другой стороны.
- [ ] При изменении контракта — отдельная запись `[ACTION: windows]`/`[ACTION: macos]`.

## 5. Текущее состояние (обновлять при изменениях)

- **Разработка продукта — только на macOS.** Windows/Linux: порт, платформенный слой, сборка.
- Версия: **1.0.6**.
- Ветка: **master**; remote `origin` — приватный GitVerse.
- Схема БД: **v40**.
- Встроенных скиллов: **38**.
- Модель эмбеддингов: **не в git и не в бандле** (с 1.0.3); ~300 МБ, приложение скачивает разово после онбординга (UI-прогресс, повтор при ошибке); без неё — только FTS-поиск.
- macOS: рабочая сборка, aarch64. **Публичный релиз 1.0.6 опубликован на GitVerse:** https://gitverse.ru/mibazarow/Project-S-local-CRM/releases/tag/v1.0.6 (tag `v1.0.6`, commit `27500d3`). Ассет `Project-S-1.0.6-macOS-ARM.dmg` — 21 842 873 байта, SHA256 `22230931a652416da017adeac86c315b128490773c2ec56a4272ed3cdd5137d8`; контрольная сумма отдельным `.sha256.txt`. Сборка ad-hoc, не нотаризована.
- Windows: **рабочая сборка 1.0.6 собрана 2026-10-23 на `27500d3`**. NSIS `Project-S-1.0.6-Windows-x64-Setup.exe` — 14 322 887 байт, SHA256 `c5ad410a98c967b4693418cd80028712765156051395ce5f19df21406d3565ac`; ZIP для GitVerse `Project-S-1.0.6-Windows-x64-Setup.zip` — 14 306 516 байт, SHA256 `70a8b0884b5333b1bb516e682d76bce70908338926dc723e13cbed0e9d7ae1de`. GitVerse не принимает `.exe`, поэтому в релизе ZIP. Оба файла лежат на рабочих столах Windows и Mac; **не установлены**, GUI не запускался. Windows-фиксы: askpass через сам бинарь, Job Objects, single-instance, WebView2 оставлен онлайн-загрузчиком.
- Windows, установленная копия: всё ещё предыдущая сборка `ce8ed5b` per-user; заменится после установки пользователем.
- Доступ к Windows VM проверен 2026-10-23: `DOCS/WINDOWS-ACCESS.md`; Windows HEAD `ada814c`, рабочее дерево чистое.
- Linux: план, реализации нет.
- Модель/ONNX: `ort-backend`, модель скачивается после онбординга (не в бандле); на Windows нужны `onnxruntime.dll` и MSVC runtime.

## 6. Windows P0 (код пишет macOS, собирает Windows)

- [x] Убрать Unix-импорты из production (`processes.rs`, `skills/exec.rs`, `cdp/mod.rs`, `ssh_tools.rs`).
- [x] Shell: Git Bash при наличии, иначе PowerShell с UTF-8; login PATH без потери TMPDIR; Python/`py` для скиллов; verbatim-пути не передаются в MSYS.
- [x] Job Objects для foreground `bash`-инструмента: KILL_ON_JOB_CLOSE + TerminateJobObject; taskkill остаётся fallback.
- [x] askpass без `sh`: на Windows приложение запускается как askpass-helper, читает пароль из private-файла и выходит до Tauri.
- [x] `HOME` → `dirs`/platform resolver; убрать macOS-пути.
- [ ] `canonicalize`/`\\?\`, длинные пути, UNC, reparse points, абсолютные пути в БД (относительные пути уже нормализуются в `/`, project root/absolute path очищаются от `\\?\`).
- [x] Путь ресурсов `exe_dir/resources/...`; 38 скиллов находятся после установки (модель с 1.0.3 не бандлится).
- [x] ~~Модель доставлена в `src-tauri/resources/models/embeddinggemma-2`~~ — с 1.0.3 не бандлится; приложение скачивает само (`download_embed_model`).
- [x] `bundle.windows`, `bundle.targets`, иконки (`icon.ico` + Square*Logo).
- [ ] Нативный Windows-titlebar / Snap Layouts.
- [x] Single-instance: `tauri-plugin-single-instance`, второй запуск фокусирует существующее окно.
- [x] WebView2: по решению пользователя оставлен онлайн-загрузчик `downloadBootstrapper`, без fixedRuntime.

Подробности: `DOCS/WINDOWS-PORT.md`.

## 7. Журнал (живой; ведёт macOS, закрытые записи удаляются)

Формат записи:

```
### YYYY-MM-DD HH:MM UTC — macOS
- Ветка: <branch>
- Коммит: <hash>
- Сделано: <кратко>
- Влияние: <на общий код/БД/контракты/сборку>
- Действие: <что сделать / user|macos> — или [DONE]
- Статус: waiting | done
```

### 2026-10-23 20:55 UTC — macOS
- Ветка: `master`
- Коммит: `27500d3` (тег `v1.0.6`)
- Сделано: Windows 1.0.6 — askpass теперь запускает сам бинарь приложения в режиме `PROJECT_S_ASKPASS_FILE` (без `sh`/`.cmd`), foreground `bash` получил Job Object с `KILL_ON_JOB_CLOSE`, добавлен `tauri-plugin-single-instance`; WebView2 оставлен онлайн-загрузчиком по решению пользователя. Собраны macOS DMG и Windows NSIS 1.0.6, оба с проверкой версии/хешей. GitVerse release id `548937735` (тег `v1.0.6`).
- Влияние: Windows-рантайм закрывает askpass, дерево процессов и второй запуск; macOS получает single-instance. GUI на Windows не проверялся (по договорённости).
- Действие: `[ACTION: user]` установить Windows 1.0.6 и проверить GUI; Linux Flatpak — следующий этап.
- Статус: waiting

### 2026-10-23 22:50 UTC — macOS
- Ветка: `master`
- Коммит: `ada814c` (тег `v1.0.5`)
- Сделано: Windows-рантайм: shell-инструмент использует Git Bash (`-c`) с явным PATH или PowerShell с UTF-8; `.sh`/`.bash` скиллы получают Git Bash и обычный (не verbatim) путь; Python-скиллы — `python`/`py -3`; относительные пути нормализуются в `/`; project/absolute path очищаются от `\\?\`. Собраны macOS DMG и Windows NSIS 1.0.5. GitVerse: release id `548937654`, тег `v1.0.5`, ассеты `Project-S-1.0.5-macOS-ARM.dmg` (21 804 979 Б, SHA256 `4a28bd40…a6eb`), `Project-S-1.0.5-Windows-x64-Setup.zip` (14 276 267 Б, SHA256 `8a468f85…ebf`; внутри EXE 14 292 714 Б, SHA256 `f0265cd8…f4c`), `.sha256.txt` и `RELEASE_NOTES_1.0.5.md`. GitVerse не принимает `.exe` и пробелы в именах ассетов.
- Влияние: Windows-сборка стала рабочей для базового сценария; macOS-поведение не изменилось; релиз 1.0.4 заменён на 1.0.5.
- Действие: `[ACTION: user]` установить Windows-версию, проверить GUI; Linux Flatpak — следующий этап.
- Статус: waiting

### 2026-10-23 19:18 UTC — macOS
- Ветка: `master`
- Коммит: `0ae7d06` (релизный тег `v1.0.3` → `3e9968f`); docs-only коммит этой записи
- Сделано: опубликован публичный релиз **macOS 1.0.3** на GitVerse — https://gitverse.ru/mibazarow/Project-S-local-CRM/releases/tag/v1.0.3 (release id `548937615`, тег `v1.0.3`, `target_commitish: master`, не draft/prerelease, `is_authorized_only: false`). Ассеты: `Project-S_1.0.3_aarch64.dmg` (id `31112`) — 21 796 376 байт, SHA256 `e11b67ac597981ecadc583091fce2dfc53048bde21b4d15298e6b9beb24bedcd` (скачан с GitVerse и сверен — совпало); `Project-S_1.0.3_aarch64.dmg.sha256.txt` (id `31116`) — 94 байта. Публичная страница релиза отвечает 200.
- Влияние: только публикация релиза и документация; код, контракты и БД не менялись. Сборка не нотаризована (первый запуск — правый клик → «Открыть»). **Платформа: только macOS (aarch64).** Windows 1.0.3 и Linux — ещё не опубликованы.
- Действие: user — Windows-сборка 1.0.3 по явной команде (§3.1) и публикация Windows-ассета в этот же тег; Linux — Flatpak позже.
- Статус: waiting

### 2026-10-23 18:56 UTC — macOS
- Ветка: `master`
- Коммит: (рабочее дерево, не закоммичено по указанию пользователя)
- Сделано: 1.0.3 — модель EmbeddingGemma 2 убрана из бандла (`bundle.resources` = только `resources/skills` в обоих конфигах; из `app:build`/`app:install` убран стейдж модели, добавлен dev-скрипт `npm run model:stage`). Скачивание на фронтенде после онбординга: store `src/state/embedModelDownload.ts` (идемпотентный, FTS-fallback), баннер `EmbedModelBootstrap` (прогресс ~300 МБ, «Повторить»/«Скрыть»), уведомление в онбординге, кнопка «Скачать модель (~300 МБ)» в Настройки → Агент; IPC `downloadEmbedModel` + событие `embed-model-progress`.
- Влияние: установщики резко меньше (macOS DMG 1.0.3 < 100 МБ против ~330 МБ с моделью); без модели поиск только по словам; контракты Rust-команд не менялись (`download_embed_model` уже был).
- Действие: user — Windows-сборка 1.0.3 по явной команде (ещё не собрана; предыдущий установщик 1.0.2 — история).
- Статус: waiting

### 2026-10-23 15:34 UTC — macOS
- Ветка: `master`
- Коммит: `0003135`
- Сделано: на Windows-хосте `git pull` `ce8ed5b` → `0003135` (master, чистое дерево) и demo-сборка `npx tauri build --config src-tauri/tauri.windows.conf.json` — успешно, ~4 мин 28 с. Впервые скомпилирована Windows-ветка `#[cfg(windows)] kill_tool_child`. NSIS-установщик `Project-S_1.0.2_x64-setup.exe`, 297 438 162 байт (283.66 MiB), SHA256 `2b5f208c20029628a21f5046afa72927d0fd9f45ee0a7389369636bdcf55d041`; разложен и сверен по хешу на рабочий стол Windows и рабочий стол Mac. Не устанавливался, GUI не запускался, процессы не убивались.
- Влияние: только упаковка/сборка Windows; macOS-код, контракты и БД не менялись (docs-only коммит). Изменение в §5.
- Действие: `[ACTION: user]` установить с рабочего стола и проверить GUI.
- Статус: waiting

### 2026-10-23 15:28 UTC — macOS
- Ветка: `master`
- Коммит: см. `git log`
- Сделано: R-24 для Windows закрыт. `platform/process.rs::kill_tool_child` под `cfg(windows)` вызывает `taskkill /PID <pid> /T /F` со скрытой консолью и заглушёнными stdout/stderr, затем `child.kill()` как fallback; `/T` обходит дерево по parent pid, поэтому `sh`, запущенная им команда и её дети (dev-сервер) умирают вместе на cancel/timeout `bash`. `tool_command` на Windows по-прежнему `sh -c` (раннер не менялся); для не-Unix/не-Windows оставлен прежний `child.kill()`.
- Влияние: только платформенная ветка, общий контракт/БД/UX не менялись; Unix-ветка не тронута. Гейты macOS: `cargo test` 1092 passed / 19 ignored, `cargo clippy --all-targets -- -D warnings` с allows из `scripts/check.sh` — чисто. Windows-ветка на macOS не компилируется.
- Действие: `[DONE]` подтверждено сборкой `0003135` (см. запись от 15:34 UTC): Windows-ветка `kill_tool_child` скомпилировалась.
- Статус: done

### 2026-10-23 15:25 UTC — macOS
- Ветка: `master`
- Коммит: `ce8ed5b`
- Сделано: на Windows-хосте `git pull` `f0280ea` → `ce8ed5b` (master, чистое дерево) и demo-сборка `npx tauri build --config src-tauri/tauri.windows.conf.json` — успешно, ~4 мин; NSIS-установщик `Project-S_1.0.2_x64-setup.exe`, 297 425 172 байт (283.65 MiB), SHA256 `A74088AA65C94349E90EA06DB7E58584B9C9BF9ABE1220A3871C2D7EBB39B2F2`; скопирован на рабочий стол Windows и на Mac, установлен per-user в `C:\Users\mb\AppData\Local\Project-S`, GUI не запускался.
- Влияние: только упаковка/сборка Windows; macOS-код, контракты и БД не менялись (docs-only коммит). Изменение в §5.
- Действие: `[ACTION: user]` запустить GUI на Windows и проверить; при установке старый запущенный экземпляр держал exe — первый тихий install упал, экземпляр закрыли, вторая установка прошла.
- Статус: done

### 2026-10-23 13:54 UTC — macOS
- Ветка: `master`
- Коммит: R-24 (см. `git log`)
- Сделано: платформенный шов `platform/process.rs` пополнился двумя хелперами для foreground-инструмента `bash`: `tool_command(command)` (Unix — `sh -c` в собственной группе процессов через `process_group(0)`, pgid == pid; не-Unix — ровно как раньше) и `kill_tool_child(child)` (Unix — `kill -KILL -<pgid>` через уже существующий `signal_group`; не-Unix — прежний `child.kill()`). `agent/harness_tools/shell_tools.rs` переведён на них: stop/таймаут рвёт всё дерево, а не только обёртку `sh`.
- Влияние: общий код и контракт платформенного шва (два новых `pub fn`); на macOS поведение успешного запуска, кодов возврата, stdout/stderr/truncation и тексты ошибок не менялись, по stop/таймауту теперь умирают внуки (dev-сервер) и освобождается порт. Гейты: `cargo test` 1092 passed / 19 ignored, `cargo clippy --all-targets -- -D warnings` с allows из `scripts/check.sh` — чисто.
- Действие: `[ACTION: windows]` только по явной команде пользователя — реализовать не-Unix ветки `tool_command`/`kill_tool_child` (`taskkill /T /F` либо Job Objects, см. §6 P0) и прогнать тесты группы на хосте; в этом срезе Windows-путь намеренно не тронут.
- Статус: waiting

### 2026-10-21 17:20 UTC — macOS
- Ветка: `master`
- Коммит: см. `git log`
- Сделано: `esaxx-rs 0.1.10` вендорен в `vendor/esaxx-rs` (тот же name/version, C++ и API не менялись) и подключён через `[patch.crates-io]` в `src-tauri/Cargo.toml`; в `build.rs` для Windows больше не задаётся `/MT` (`static_crt`), macOS/Linux-поведение прежнее. Причина: LNK2038/LNK2005 — `cc` собирал esaxx со static CRT, а prebuilt ONNX Runtime (`ort-sys`) идёт с dynamic CRT (`/MD`).
- Влияние: сборка Windows; macOS `cargo check`/`cargo test` (1068 passed), `npm test` (532), `npm run build` — зелёные.
- Действие: `[ACTION: macos]` на Windows после pull собрать demo и убедиться, что LNK2038/LNK2005 ушли; если появятся новые CRT-конфликты — смотреть другие `cc`-крейты (`sqlite-vec`) с тем же паттерном.
- Статус: waiting

### 2026-10-21 17:10 UTC — macOS
- Ветка: `master`
- Коммит: см. `git log`
- Сделано: минимальный шов для компиляции Windows demo: `platform/mod.rs` (`home_dir`, `default_working_dir`, `is_executable`, `restrict_permissions`, `hide_console`); `processes.rs` — cfg-ветки Unix/Windows (`cmd /C`, `CREATE_NO_WINDOW`, `taskkill /T /F`, `tasklist` вместо `kill -0`), macOS-поведение не менялось; `ssh_tools`/`skills/exec`/`cdp` — permissions/executable через шов; `dirs` для HOME/Documents; ресурсы скиллов и модели ищутся в `exe_dir/resources`; добавлены `src-tauri/tauri.windows.conf.json` (NSIS, WebView2 downloadBootstrapper) и `scripts/windows/stage-embed-model.ps1`.
- Влияние: общий код и сборка; macOS-тесты/сборка зелёные (1 известный flaky-тест `fs_watch_quiet` падает и на HEAD). Windows-паритета нет: `askpass.sh`, `sh`-инструменты, macOS harness tools, ONNX DLL и модель на хосте — runtime-риски demo.
- Действие: `[ACTION: macos]` после push собрать на Windows по SSH: `git pull; npm ci; npx tauri build --config src-tauri/tauri.windows.conf.json`; `[ACTION: windows]` нет (хост только собирает).
- Статус: waiting

### 2026-10-21 16:35 UTC — macOS
- Ветка: `master`
- Коммит: см. `git log`
- Сделано: Windows-агента нет. Windows — только build/test-хост; весь код, включая `platform/`, пишет macOS. Сборка — только по команде пользователя через SSH; Windows — demo-only.
- Влияние: процесс/scope; код не менялся.
- Действие: `[ACTION: user]` дать SSH-доступ (alias/host/user/port/path); `[ACTION: macos]` после доступа проверить окружение и собрать demo.
- Статус: waiting

### 2026-10-21 17:25 UTC — macOS
- Ветка: `master`
- Коммит: `f0280ea`
- Сделано: Windows demo-сборка успешна после патча `esaxx-rs` CRT; NSIS установщик 283.67 MiB; скопирован на рабочий стол; установлен per-user; не запускался.
- Влияние: Windows build; код macOS не менялся.
- Действие: `[DONE]` сборка/установка; `[ACTION: user]` запустить GUI на Windows и проверить.
- Статус: done

## 8. Открытые вопросы

- [ ] Переименовывать `master` → `main`? Сейчас оставляем `master`, чтобы не плодить ветки; решение — до начала Windows-работы.
- [ ] Основной Windows-установщик: NSIS per-user или MSI per-machine?
- [ ] ARM64 Windows — нужен?
- [ ] Секреты на Windows: plaintext JSON (паритет) или Credential Manager/DPAPI?
- [ ] Модель и ONNX DLL: в установщик, докачивать при первом запуске или отдельный пакет?
- Подробности и остальные вопросы — `DOCS/WINDOWS-PORT.md` §14.

## 9. Ссылки

- `AGENTS.md` — правила работы агентов.
- `README.md` — презентация продукта, запуск; `DOCS/about.md` — архитектура, сборка, модель после клонирования.
- `DOCS/WINDOWS-PORT.md` — статус и остаток работ по Windows.
- `DOCS/LINUX-PORT.md` — план Linux.
- `DOCS/Q.md` — текущая очередь задач.

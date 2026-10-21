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
> 12. **На Windows нет агента.** Windows — только build/test-хост: `git pull`, сборка, тесты, артефакты. Весь код, включая `platform/windows`, пишет macOS-агент; на Windows код не правится и не коммитится.

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
4. **WebView2:** baseline **109.0.1518.78** (Tauri 2); ниже/нет — fixed runtime. Сверить с release notes Tauri перед релизом.
5. **Ресурсы:** единый `resource_dir()` + fallback `exe_dir/resources` для dev. macOS добавляет helper; `paths.rs` — `notes_dir` и `harness_inbox_dir`. `package.json` — кросс-платформенный `scripts/stage-embed-model.mjs`, bash-обёртка остаётся.

## 0.2. Режим работы: macOS по умолчанию, Windows по команде

- **Обычный режим:** работаем только на macOS — фичи, полировка, дизайн, функционал, тесты, релиз macOS.
- **Windows-режим:** включается **только явной командой пользователя** («а теперь делаем для Windows»). Никаких автоматических Windows-сборок на каждый push или после каждой macOS-правки.
- **Windows — только demo-сборка.** Боевых данных и production-релиза на Windows нет. Подпись, updater, миграция данных, Credential Manager, Recycle Bin и полный паритет инструментов **не блокируют demo**; там всё проще.
- **По команде:** macOS-агент подключается к Windows по SSH, делает `pull`/merge `master`, применяет изменения, собирает, гоняет тесты, забирает артефакты и пишет отчёт. Пользователь проверяет GUI на Windows.
- **Windows-сессия сама не начинает работу**: ждёт команду пользователя или `[ACTION: windows]` в этом журнале.
- CI на каждый push для Windows не обязателен; сборка — on-demand, пока не решим иначе.

## 1. Роли и ветки

| Роль | Где | Ветка | Что делает |
|---|---|---|---|
| macOS-агент | Mac | `master` + короткие `feat/*`, `fix/*` | **Единственный разработчик**: продукт, `platform/windows`, сборка, релизы macOS |
| Windows build host | Windows | — | **Нет агента.** Только `git pull`, сборка, тесты, артефакты. Код не правится и не коммитится |
| Linux build host (позже) | Linux | — | **Нет агента.** Только сборка |

> **Сборка Windows — с macOS по SSH (основной путь).** Windows-хост — это build/test target: macOS-агент подключается по SSH, делает `pull`, сборку, тесты и забирает артефакты. Windows-агента нет; пользователь только просит собрать и проверяет GUI.

Правила:

- `master` — единый источник правды и защищённая ветка. Все изменения — через PR/merge.
- Постоянных веток `macos`/`windows`/`linux` и временной `port/windows` **нет**. Весь код, включая `platform/windows`, пишет macOS-агент в `feat/*`/`fix/*` или `master`.
- Windows build host не коммитит: только `git pull`, сборка, тесты, артефакты. Все коммиты и push делает macOS.
- Релизные теги (`v1.0.2`) ставятся на `master`; артефакты собираются под каждую ОС из одного коммита.
- Если Windows временно отстаёт — используем pre-release-тег (`v1.0.3-win.1`), а не отдельную версию кода.

## 2. Код: платформенный слой, а не форк

```
src-tauri/src/platform/
  mod.rs            # трейты/контракты: Shell, Processes, Clipboard, Notifications,
                    # UiAutomation, Trash, Secrets, OpenWith, Paths, Screenshot, OCR, PDF
  macos/
  windows/
  linux/            # позже

src-tauri/tauri.macos.conf.json
src-tauri/tauri.windows.conf.json
src-tauri/tauri.linux.conf.json

scripts/macos/
scripts/windows/
scripts/linux/
```

- Общая бизнес-логика (задачи, чаты, БД, агент, скиллы, память, поиск) — **одна** для всех ОС.
- `platform/windows` пишет macOS-агент; Windows-хост только компилирует, тестирует и отдаёт артефакты. Кода на Windows нет.
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
3. Работать в своей ветке на macOS: продукт и весь код, включая `platform/windows`.
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
ssh win 'powershell -NoProfile -Command "cd C:\path\Project-S; git pull; npm ci; npm run tauri build"'
```

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
- Версия: **1.0.2**.
- Ветка: **master**; remote `origin` — приватный GitVerse.
- Схема БД: **v40**.
- Встроенных скиллов: **38**.
- Модель эмбеддингов: **не в git**, ~330 МБ; доставляется до сборки (см. `README.md` → «После клонирования»).
- macOS: рабочая сборка, aarch64.
- Windows: **только demo-сборка** (без боевых данных и production-hardening); минимальный платформенный шов для компиляции есть (`platform/mod.rs`, cfg-ветки, Windows-конфиг), полного порта нет; сборка — с macOS по SSH (см. §3.1). **Windows-агента нет**; код пишет macOS.
- Доступ к Windows VM проверен 2026-10-21: `DOCS/WINDOWS-ACCESS.md`; Windows HEAD `d448c15`, рабочее дерево чистое.
- Linux: план, реализации нет.
- Модель/ONNX: `ort-backend`, модель в бандле; на Windows нужны `onnxruntime.dll` и MSVC runtime.

## 6. Windows P0 (код пишет macOS, собирает Windows)

- [x] Убрать Unix-импорты из production (`processes.rs`, `skills/exec.rs`, `cdp/mod.rs`, `ssh_tools.rs`).
- [ ] Job Objects + kill tree вместо process group/`kill -0`.
- [ ] Убрать `sh`/`bash -lc`; PowerShell/cmd, quoting, `CREATE_NO_WINDOW`.
- [x] `HOME` → `dirs`/platform resolver; убрать macOS-пути.
- [ ] `canonicalize`/`\\?\`, абсолютные пути в БД, reparse points.
- [ ] Путь ресурсов `exe_dir/resources/...`; 38 скиллов находятся.
- [ ] Модель доставлена в `src-tauri/resources/models/embeddinggemma-2` (4 файла); PowerShell-скрипт или ручная копия.
- [ ] `bundle.windows`, `bundle.targets`, иконки, titlebar, single-instance.

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
- Сделано: Windows-агента нет. Windows — только build/test-хост; весь код, включая `platform/windows`, пишет macOS. Сборка — только по команде пользователя через SSH; Windows — demo-only.
- Влияние: процесс/scope; код не менялся.
- Действие: `[ACTION: user]` дать SSH-доступ (alias/host/user/port/path); `[ACTION: macos]` после доступа проверить окружение и собрать demo.
- Статус: waiting

## 8. Открытые вопросы

- [ ] Переименовывать `master` → `main`? Сейчас оставляем `master`, чтобы не плодить ветки; решение — до начала Windows-работы.
- [ ] Основной Windows-установщик: NSIS per-user или MSI per-machine?
- [ ] ARM64 Windows — нужен?
- [ ] Секреты на Windows: plaintext JSON (паритет) или Credential Manager/DPAPI?
- [ ] Модель и ONNX DLL: в установщик, докачивать при первом запуске или отдельный пакет?
- Подробности и остальные вопросы — `DOCS/WINDOWS-PORT.md` §14.

## 9. Ссылки

- `AGENTS.md` — правила работы агентов.
- `README.md` — запуск, модель после клонирования.
- `DOCS/WINDOWS-PORT.md` — полный план порта.
- `DOCS/LINUX-PORT.md` — план Linux.
- `DOCS/Q.md` — текущая очередь задач.

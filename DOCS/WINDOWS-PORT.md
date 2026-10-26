# WINDOWS-PORT — Windows 11: текущее состояние и остаток работ

Статус: **сборка под Windows 11 x64 работает**. Релиз **1.1.0** собран 2026-10-26 на коммите `74b9da6`: ZIP для GitVerse `Project-S-1.1.0-Windows-x64-Setup.zip` — 14 488 398 байт, SHA256 `026f9e7d7b0910d54b8e251159ec82ce53f7d0c95e490ae70e15911651b23870`; внутри NSIS `Project-S 1.1.0 Windows x64 Setup.exe` — 14 505 047 байт, SHA256 `ca4b8a1426d723ad38a46b96c31cae78d75da6ae7cdf20430e7520d3e683fefb`. Файлы лежат на рабочих столах Windows и Mac, **не установлены**; установленная копия — предыдущая сборка. GUI после нового установщика **не проверен**. На Windows ONNX Runtime работает на CPU; CoreML-провайдер — только macOS. Askpass, Job Objects, single-instance и shell исправлены; WebView2 по решению пользователя остаётся онлайн-загрузчиком. Осталось: длинные/UNC-пути, macOS-only инструменты и production-hardening (подпись, updater).

Scope: **Windows — рабочая сборка для локального использования**; боевые данные и production-hardening (подпись, updater, Credential Manager, Recycle Bin, полный паритет инструментов) остаются отдельным этапом.

> **Агента на Windows нет.** Весь код, включая `src-tauri/src/platform/`, пишет macOS-агент. Windows-хост только делает `git pull`, сборку, тесты и отдаёт артефакты; код там не правится и не коммитится. Сборка — по явной команде пользователя, с macOS по SSH (`CROSPLATFORM.md` §3.1).

---

## 1. Что уже работает (сделано)

| Область | Что сделано |
|---|---|
| Платформенный шов | `src-tauri/src/platform/` — два файла: `mod.rs` и `process.rs`. Отдельных каталогов `platform/<os>` нет; различия ОС — cfg-ветками внутри этих двух файлов |
| Job Objects | foreground `bash`-инструмент получает Job Object с `KILL_ON_JOB_CLOSE`; при cancel/timeout — `TerminateJobObject` |
| Дом и рабочая папка | `dirs` вместо `HOME`: `home_dir()`, `default_working_dir()` = `dirs::document_dir()/PROJECT-S` |
| Исполняемость и права | `is_executable` (Unix — бит, Windows — расширения), `restrict_permissions` (Windows — no-op вместо `PermissionsExt`) |
| Консоль | `hide_console` → `CREATE_NO_WINDOW` на Windows: окна не мигают |
| Файловый менеджер | `reveal_in_file_manager` → `explorer.exe /select,` |
| Фоновые команды | `background_command` → `cmd /C` + скрытая консоль |
| Живость процессов | `kill_alive` / `liveness_target` / `signal_group` → `taskkill` + `tasklist` вместо `kill -0`/PGID |
| bash-инструмент | Git Bash (`-c`) с явным PATH или PowerShell с UTF-8; foreground-процесс в Job Object с `KILL_ON_JOB_CLOSE` + `TerminateJobObject`, `taskkill /T /F` остаётся fallback |
| Askpass | Windows OpenSSH запускает сам `project-s.exe` в режиме `PROJECT_S_ASKPASS_FILE`: пароль читается из private-файла, процесс выходит до Tauri/GUI |
| Single-instance | `tauri-plugin-single-instance`: второй запуск фокусирует существующее окно |
| Скиллы | `.sh`/`.bash` — Git Bash с обычным (не verbatim) путём; `.py` — `python` или `py -3`; `node` для `.js`/`.mjs` |
| Пути | относительные пути нормализуются в `/`; project root и absolute path очищаются от `\\?\`; длинные/UNC/reparse остаются в P0 |
| Компиляция | Полная сборка под `x86_64-pc-windows-msvc` проходит на Windows-хосте (по SSH); Unix-код разведён по cfg. На macOS Windows-ветка не компилируется — проверка только на хосте |
| Упаковка | `src-tauri/tauri.windows.conf.json`: `targets: ["nsis"]`, `installMode: currentUser`, WebView2 `downloadBootstrapper` (silent), ресурсы `resources/skills` (модель с 1.0.3 не бандлится) |
| Ресурсы | скиллы ищутся в `exe_dir/resources/...`; модель эмбеддингов — в `<workingDir>/Settings/models/embeddinggemma-2/`, скачивается приложением |
| Доставка модели | с 1.0.3 — само приложение после онбординга (команда `download_embed_model`, HF `onnx-community/embeddinggemma-2-ONNX`, ~300 МБ, UI-прогресс, FTS-fallback); `scripts/windows/stage-embed-model.ps1` — только для разработчиков |
| CRT-патч | `esaxx-rs 0.1.10` вендорен в `vendor/esaxx-rs` (`[patch.crates-io]`), в `build.rs` для Windows убран `static_crt(true)` — иначе LNK2038/LNK2005 против dynamic CRT (`/MD`) у prebuilt ONNX Runtime. Не возвращать |
| Сборка по SSH | рабочий путь: macOS-агент по SSH делает `pull` → `npm ci` → `npx tauri build --config src-tauri/tauri.windows.conf.json`, артефакты забирает на Mac |
| Единственное предупреждение сборки | нефатальное `esaxx-rs` `-std=c++11` D9002 |

Не сделано и не проверено: **GUI на Windows после нового установщика**. Установленная копия — `ce8ed5b`. Shell, Python-скиллы и базовые пути уже исправлены и собраны.

### 1.1. Что в собранной сборке заведомо не работает (runtime-риски)

Это не «мелочи», а ожидаемое поведение текущей сборки — чтобы не чинить их задним числом под видом багов:

- macOS-инструменты харнеса (clipboard image, OCR, PDFKit, скриншоты, `open_with`, `mdfind`, `launchd`, `trash_paths`, `keychain_get`, `defaults_write`, `archive`) на Windows не портированы — часть из них вообще не вызывается, часть даёт ошибку.
- ONNX-эмбеддинги: `onnxruntime.dll` и MSVC runtime должны быть на машине; семантический поиск деградирует до FTS, если модель/бинарники не на месте.
- Длинные пути/UNC/reparse points и часть абсолютных путей в БД ещё не покрыты.

## 2. Остаток P0

| # | Задача | Где сейчас | Что не так |
|---|---|---|---|
| P0-5 | Пути | относительные/абсолютные пути уже нормализуются на границе | Остались длинные пути (manifest + `LongPathsEnabled`), UNC/сетевые шары, junctions/reparse points в `path_policy`, зарезервированные имена (`CON`, `NUL`, хвостовые точки/пробелы) |
| P0-6 | WebView2 | `downloadBootstrapper` | Решено: онлайн-загрузчик, без fixedRuntime/offlineInstaller. UDF/проверка наличия остаются в общем smoke-test. |

### 2.1. WebView2: что проверить после фиксации режима

Baseline Tauri 2 — **109.0.1518.78** (`CROSPLATFORM.md` §0.1); ниже или отсутствует — fixed runtime. Проверять несколько источников наличия (HKLM/HKCU `EdgeUpdate\Clients\{F3017226-...}` → `pv`, файл `msedgewebview2.exe`), результат писать в лог и в «О приложении». Дальше:

- Рендер: CSS/шрифты/скроллбары, `backdrop-filter`, `position: sticky`, SVG.
- `localStorage` на origin `http://tauri.localhost` (на macOS — `tauri://localhost`): ключи `project-s.tasks.view.<projectId>` и `project-s:lastProject:<encoded workingDir>` должны читаться; IndexedDB не используется.
- Drag-and-drop, clipboard, file input, PDF/iframe.
- Стриминг SSE и долгие запросы (агентский чат).
- DevTools только в dev-сборке; в release отключить.
- `prefers-reduced-motion`, high contrast, масштаб 125/150/200%.
- Ошибки WebView2 (GPU, sandbox, профиль) — понятный экран с кнопкой «Перезапустить» и ссылкой на логи; антивирус/OneDrive могут лочить файлы профиля.

## 3. Установщик и релиз (осталось)

- **Формат:** NSIS per-user работает. Открытый вопрос — нужен ли MSI/WiX per-machine для корпоративной поставки.
- **Подпись:** Authenticode для exe/установщика + timestamp нет. Без подписи SmartScreen показывает предупреждение; EV-сертификат снимает его сразу, обычный — копит репутацию со временем.
- **SmartScreen/Defender:** прогнать установщик на чистой машине, при ложных срабатываниях — отправка в Microsoft.
- **Updater:** отсутствует. Нужен канал (Tauri updater или внешний), подпись манифеста, откат, миграции схемы при обновлении.
- **Uninstall:** не должен удалять рабочие папки проектов и пользовательские данные; чистить ярлыки/автозапуск.
- **Автозапуск:** `HKCU\...\Run` или Task Scheduler; учитывать отключение пользователем.
- **Deep links:** регистрация протокола + передача аргументов в уже запущенный экземпляр (single-instance уже есть, нужна обработка argv).
- **Размер:** установщик 1.1.0 — 13.83 MiB; модель (~300 МБ) не в бандле и скачивается после онбординга. WebView2 — онлайн-загрузчик, fixedRuntime не используем. Эмбеддинги на Windows считаются на CPU (CoreML — только macOS).
- **CI:** отсутствует; сборка on-demand по SSH. `scripts/install-app.sh` (bash + `ditto`/`xattr`) на Windows неприменим, `npm run app:build`/`app:install` на Windows не использовать.

## 4. Данные, пути, миграция

- Модель хранения не менялась: всё под `<workingDir>/Settings/` и `<workingDir>/Harness/`; в app config dir только `working_dir.txt` и `license-accepted.json` → на Windows `%APPDATA%\app.project-s.ru`.
- Дефолт рабочей папки сделан через `dirs::document_dir()` (сделано).
- В БД абсолютные пути (`projects.path`, `trash.*`, `versions.*`, `message_attachments.saved_path`, `processes.cwd/log_path`, таблицы индекса) — при переносе с macOS их нужно переписать и провалидировать при старте.
- Схема **v40**, SQLite WAL + FTS5 + sqlite-vec; миграции общие, платформенных миграций не делать.
- Тексты: UTF-8 без BOM, CRLF/LF не менять без причины; санитайзинг имён заметок/бэкапов/экспортов под Windows.
- Бэкап ZIP: `Settings/config`, `Settings/models`, `Settings/skills`, `.trash`, `.versions` исключены — проверить, что ZIP читается Explorer и 7-Zip.
- Три слоя «корзины» не путать: корзина приложения (`Settings/.trash/`, 30 дней), безвозвратное удаление проекта, `trash_paths` → системная корзина. Windows-корзина (Recycle Bin) не интегрирована — отдельное решение.
- Миграция macOS → Windows: отдельный сценарий (экспорт/импорт или перенос папки + ремонт путей), тестировать на копии данных.

## 5. Паритет инструментов (только остаток)

Сделано: `fs_reveal_in_finder` → `explorer /select,`; фоновые процессы/живость/kill (`taskkill`/`tasklist`); дерево `bash`-инструмента (Job Object + `taskkill` fallback); askpass; single-instance.

| Инструмент | macOS сегодня | Windows: осталось |
|---|---|---|

| `clipboard_get_image` | osascript PNGf/TIFF + `sips` | Win32 clipboard `CF_DIB`/`CF_PNG` + `image` |
| Скриншот | `screencapture` | GDI/BitBlt или Windows Graphics Capture, Per-Monitor V2 DPI |
| OCR | Apple Vision (Swift) | Windows.Media.Ocr / Tesseract / vision-провайдер LLM |
| PDF (merge/split/`pdf_to_image`) | PDFKit, `pdftoppm`/`sips` | PDFium/Poppler; лицензии и размер |
| `image_transform` | `sips` | crate `image` |
| `open` / `open_with` | `/usr/bin/open`, NSWorkspace | `ShellExecuteEx`/`start`, ассоциации |
| `launchd` | `launchctl` + LaunchAgents | Task Scheduler или папка Startup |
| `mdfind` | Spotlight | Windows Search или свой индекс; паритет не 1:1 |
| `trash_paths` / `restore_trashed` | `~/.Trash` + ledger | Recycle Bin (`IFileOperation`) + ledger |
| `keychain_get` | `security` CLI | Credential Manager/DPAPI; формат секретов другой |
| `defaults_write` | `defaults` CLI | `reg.exe`/Registry API, не писать в чужие ветки |
| `archive` | `zip`/`unzip`/`tar`/`7z` | `tar.exe` или Rust `zip`/`tar`/`flate2` |
| `fs_watch` | `notify` | `ReadDirectoryChangesW`; OneDrive, AV, переполнение буфера |
| `sysinfo` | `sysinfo` + `pmset`/`route` | `sysinfo` + `GetSystemPowerStatus`/`GetBestRoute2` |
| `notify` | osascript notification | Toast + AppUserModelID; fallback для click/actions |
| `macos_ui` | osascript / System Events | UI Automation + `SendInput`; UIPI: из non-elevated elevated-окна не тронуть |
| Браузер / CDP | macOS-пути Chrome | пути Chrome/Edge в `Program Files`/`%LOCALAPPDATA%`, отдельный профиль |
| Tray / autostart | — | иконка, меню, DPI; реестр/Task Scheduler |

## 6. UI/UX

- **Шрифты:** UI рассчитан на SF Pro, распространять его нельзя. Остается: Segoe UI Variable/Segoe UI/Inter; проверить кириллицу и метрики.
- **Titlebar:** нативного Windows-титулбара и Snap Layouts нет. Решение: стандартный титулбар или custom с учётом Snap Layouts.
- **DPI/zoom:** UI-масштаб 100/110/130/175 через `setZoom` складывается с OS DPI 125/150/200 — проверить отсутствие двойного масштабирования, выход окна за экран, multi-monitor, RDP.
- **Горячие клавиши:** Cmd → Ctrl, системное меню/Alt, контекстные меню.
- **DnD из Explorer:** OLE/`CF_HDROP`, копирование (не перемещение), длинные пути, UIPI.
- **Уведомления:** toast + AUMID, Focus Assist, клик/действия и понятный fallback.
- **Tray:** иконка, меню, левый/правый клик, тёмная тема, DPI.
- **WebView2 origin:** на macOS `tauri://localhost`, в WebView2 `http://tauri.localhost`; `localStorage` по origin не переносится. Ключи: `project-s.tasks.view.<projectId>`, `project-s:lastProject:<encoded workingDir>` — проверить чтение в Windows-сборке. IndexedDB не используется.
- **Тёмная тема:** не включать автоматически без отдельного решения.

## 7. Секреты и безопасность

- Как есть (паритет с macOS, для локальной сборки приемлемо): API-ключи открытым текстом в `Settings/config/llm.json`, MCP env/headers в `mcp.json`, SSH-пароли в `ssh.json` и `datas.md`.
- `license-accepted.json` — обычный JSON без anti-tamper; не менять.
- CSP в `tauri.conf.json` — `null`.
- `keychain_get` — только агентский инструмент чтения, для секретов приложения не используется.
- Askpass-файл на Windows лежит в private-каталоге профиля и наследует его ACL; отдельный `icacls`-хардening, Credential Manager/DPAPI — будущая работа, меняет формат хранения и требует миграции.

## 8. Тестовая матрица и команды сборки

Команды (с macOS по SSH, доступ — `DOCS/WINDOWS-ACCESS.md`; `<repo>` — путь репозитория на Windows-хосте):

```bash
ssh win11 'powershell -NoProfile -ExecutionPolicy Bypass -File <repo>\scripts\windows\stage-embed-model.ps1'
ssh win11 'powershell -NoProfile -Command "cd <repo>; git pull; npm ci; npx tauri build --config src-tauri/tauri.windows.conf.json"'
scp -r "win11:<repo>/src-tauri/target/release/bundle" ./artifacts/windows/
```

Конфиг обязателен: без `--config src-tauri/tauri.windows.conf.json` Tauri берёт macOS-конфиг. Установщик: `src-tauri\target\release\bundle\nsis\Project-S_1.1.0_x64-setup.exe`; тихая установка per-user: `Start-Process <installer> -ArgumentList '/S' -Wait`. Проверка артефакта: `Get-FileHash` по SHA256.

Матрица:

- Windows 11 24H2/25H2, x64; обычный пользователь и админ; доменные политики/AppLocker — если решаем поддерживать.
- WebView2: есть / нет / старый / битый; после смены режима — fixedRuntime и офлайн-установка.
- UI zoom 100/110/130/175 × OS DPI 125/150/200; 2 монитора; RDP.
- Пути: кириллица, пробелы, длинные пути, OneDrive, сетевой диск, read-only, reparse points.
- Модель: первый запуск без сети, `onnxruntime.dll` + MSVC runtime, повторный запуск, повреждённый файл.
- Скиллы: 38 встроенных находятся из `exe_dir/resources`, sync/cleanup после обновления, `python`/`py`, отсутствие `bash`.
- Сеть: offline, proxy, MITM, firewall.
- Антивирус: Defender и сторонний, локи файлов при удалении проекта.
- Уведомления: toast, клик, действия, Focus Assist, непрочитанные, сон/пробуждение.
- Сценарии GUI: лицензия, онбординг, проект, «Мои задачи», Kanban/List, заметки/файлы, чат/агент, очередь/stop, расписание, настройки, бэкап/восстановление, архив, удаление проекта, обновление, удаление приложения.
- **Первый прогон GUI на установщике 1.1.0 ещё не сделан** — это следующий шаг после ручной установки.

## 9. Открытые вопросы

- [ ] Основной установщик: NSIS per-user или MSI per-machine?
- [ ] WebView2: `fixedRuntime` всегда или системный + fallback? Путь UDF?
- [ ] Допустим ли размер установщика с фиксированным runtime? Бюджет? (С 1.0.3 модель не в бандле — установщик 1.1.0 ~13.83 MiB.)
- [ ] Секреты: оставить plaintext JSON (паритет) или Credential Manager/DPAPI? Как мигрировать?
- [ ] `onnxruntime.dll`: в установщике или отдельным пакетом? (Модель решена в 1.0.3: скачивается приложением после онбординга.)
- [ ] Автообновление: Tauri updater или внешний канал; какой канал (stable/beta)?
- [ ] Кодовая подпись: обычный или EV сертификат, кто владелец?
- [ ] ARM64 (`aarch64-pc-windows-msvc`) — нужен? Тогда отдельные ONNX-бинарники.
- [ ] Microsoft Store/MSIX — нужен ли канал?
- [ ] Recycle Bin вместо своей корзины?
- [ ] OCR: Windows.Media.Ocr, Tesseract или только vision-провайдер?
- [ ] SSH: требовать OpenSSH Client или нести свой?
- [ ] Браузер: требовать Chrome/Edge или оставить опциональным?
- [ ] Ассоциации файлов — нужны ли вообще?
- [ ] Доменные политики/AppLocker — обязательны?
- [ ] Миграция с macOS: автоматическая или экспорт/импорт?
- [ ] CSP: оставить `null` или ввести политику?

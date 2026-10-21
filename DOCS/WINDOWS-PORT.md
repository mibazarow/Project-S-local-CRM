# WINDOWS-PORT — перенос Project-S на Windows

Статус: **планирование, реализация не начата**. Актуализировано **2026-10-21** под **v1.0.2**. Рабочая сборка существует только под macOS (aarch64). Цель — паритет 1:1 с текущей macOS-версией: дизайн, функционал, поведение, данные, агентский харнес. Базовый стек сохраняем: **Tauri 2 + React 19 + TypeScript + Rust + SQLite**. Меняем платформенный слой и упаковку.

> Правило: сначала пытаемся использовать системный WebView2. Если его нет, он старый или сломан — приложение обязано работать на своём встроенном runtime. Никаких «скачайте WebView2 вручную» без запасного пути.

---

## 0. Актуализация 2026-10-21 / текущее состояние v1.0.2

### 0.1. Что изменилось с прошлой версии плана

Прошлый план писался под более раннюю поверхность приложения. С тех пор появились «Мои задачи», лицензионный гейт, встроенная ONNX-модель, пер-модельные контекстные окна, новые агентские инструменты и т.д. При этом главное изменение — не в фичах, а в исходной точке:

- **Платформенной абстракции нет вообще.** В production-коде **ноль** `#[cfg(target_os = "macos")]` и нет модуля `platform/`. macOS-специфика размазана по бизнес-логике.
- **Windows-сборка сейчас не собирается и не запустится** без отдельного платформенного слоя. Это не вопрос UX-полировки: часть кода не компилируется под Windows, часть гарантированно падает в рантайме.
- **Поверхность приложения выросла**: 38 встроенных скиллов, встроенная модель ~330 МБ, схема БД v40, 13 новых «компьютерных» инструментов, каскадное удаление, планировщик, фоновый чат.

Вывод: раздел «что делать» прошлого плана в основном остаётся верным, но **объём работ и стартовая точка другие**. Оценку 6–10 недель нужно пересматривать (см. §15).

### 0.2. Базовая линия

| Параметр | Значение (2026-10-21) |
|---|---|
| Версия приложения | **1.0.2** |
| Схема БД | **v40** (SQLite + FTS5 + sqlite-vec) |
| Встроенные скиллы | **38** |
| Встроенная модель | EmbeddingGemma 2 ONNX, **~330 МБ** |
| Модель в git | **нет**; доставляется отдельно до сборки (см. §0.4.1) |
| Размер приложения | **~390 МБ** |
| Размер DMG | **~300 МБ** |
| Архитектура macOS-сборки | **только aarch64**; universal/x86_64 нет |
| Платформенный слой | отсутствует |
| CI | отсутствует |
| Кодовая подпись / notarization | конфигурации нет |
| Автообновление | отсутствует |
| Single-instance / deep-link | отсутствуют |
| `bundle.targets` | только macOS |
| `bundle.windows`, режим WebView2, UDF | не настроены |
| CSP в `tauri.conf.json` | `null` |

### 0.3. Пять блокеров первой сборки

Часть блокеров — компиляция, часть — гарантированный отказ в рантайме. Все пять нужно снять до разговора о паритете.

| # | Блокер | Где | Тип | Суть |
|---|---|---|---|---|
| 1 | Unix process API | `src-tauri/src/processes.rs` | компиляция | `std::os::unix::process::CommandExt`, `process_group(0)`, `kill -0`, работа с PGID — на Windows этих API нет |
| 2 | Unix permissions | `skills/exec.rs`, `cdp/mod.rs`, `ssh_tools.rs` | компиляция | `PermissionsExt`, файлы `0600`, скрипты `0700` |
| 3 | POSIX shell | те же модули | рантайм | `sh`, `bash`, `bash -lc`; на Windows bash отсутствует |
| 4 | `HOME` и macOS-пути | `path_policy`, onboarding, skills, ssh | рантайм | `HOME` как резолвер домашней папки; `/Users`, `/Applications`, `/System/Library/Fonts`, `/Library/Fonts`, `/opt/homebrew`, `/usr/local`, `~/.Trash`, `~/Library/LaunchAgents`, `/usr/bin/...` |
| 5 | Пути и ресурсы | БД, `canonicalize`, скиллы | рантайм | `canonicalize` на Windows отдаёт `\\?\C:\...`; в БД лежат абсолютные пути; junctions/reparse points обрабатываются не полностью; путь ресурсов скиллов на Windows другой |

Дополнительно: `std::env::var("HOME")` используется как единственный домашний резолвер, крейт `dirs` не подключён.

### 0.4. Фактическое хранение данных

Почти всё живёт **не** в системной папке конфигов, а внутри рабочей папки:

```
<workingDir>/
  Settings/
    config/{llm,app,indexing,mcp,ssh,openWith,templates}.json
    .db/project-s.db                 SQLite, FTS5, sqlite-vec, schema v40
    models/embeddinggemma-2/         ~330 МБ
    skills/{builtin,user}/           enabled.json, .builtin-version
    .trash/                          корзина приложения (файлы/задачи/заметки, 30 дней)
    .versions/
    tmp/
    backups/
    logs/                            журнал отключён
  Harness/
    tmp/processes/                   логи процессов
  Projects/                          включая Projects/.my-tasks
```

В системной папке конфигов приложения лежат **только два файла**:

- `working_dir.txt`
- `license-accepted.json`

macOS: `~/Library/Application Support/app.project-s.ru` (debug — `.../dev`). На Windows идентификатор тот же, значит **`%APPDATA%\app.project-s.ru`**.

Рабочая папка по умолчанию — **захардкоженный** `$HOME/Documents/PROJECT-S`, а не `dirs::document_dir()`. На Windows это будет `%USERPROFILE%\Documents\PROJECT-S` только потому, что так получится из `HOME`/профиля, а не из API документов.

Абсолютные пути в БД: `projects.path`, `trash.*`, `versions.*`, `message_attachments.saved_path`, `processes.cwd/log_path`, таблицы индекса.

### 0.4.1. Модель эмбеддингов отсутствует в git — доставить до сборки

> **Для агента, который клонировал репозиторий:** если в `src-tauri/resources/models/embeddinggemma-2/` нет `model_quantized.onnx_data`, это ожидаемо. Модель (~330 МБ) намеренно не хранится в git. Её нужно доставить до сборки, иначе приложение соберётся без семантического поиска (останется FTS), а релизный бандл будет неполным.

Нужные файлы в `src-tauri/resources/models/embeddinggemma-2/`:

- `model_quantized.onnx`
- `model_quantized.onnx_data`
- `tokenizer.json`
- `tokenizer_config.json`

Откуда взять:

- с Mac — из демо-хранилища `~/Documents/PROJECT-S/Settings/models/embeddinggemma-2/`;
- или скачать из Hugging Face `onnx-community/embeddinggemma-2-ONNX`: файлы `onnx/model_quantized.onnx`, `onnx/model_quantized.onnx_data`, `tokenizer.json`, `tokenizer_config.json`.

`scripts/stage-embed-model.sh` — bash/macOS; источник переопределяется через `PROJECT_S_MODEL_SRC`. Для Windows нужен PowerShell-аналог (`stage-embed-model.ps1`) или Git Bash/WSL; нативная Windows-сборка без этого шага модель не получит. `src-tauri/resources/models/` игнорируется git, поэтому после клонирования там будет только `.gitkeep`.

Проверка после доставки: все четыре файла ненулевые, `model_quantized.onnx_data` ~299 МБ, `tokenizer.json` ~31 МБ. Без модели приложение не падает, но `embed_status` покажет отсутствие модели, а поиск деградирует до FTS.

### 0.5. Секреты и безопасность (как есть)

Сейчас это важно зафиксировать, потому что на Windows придётся решать, что делать с секретами:

- API-ключи лежат **открытым текстом** в `Settings/config/llm.json` (верхний уровень и провайдеры).
- MCP env/headers — открытым текстом в `mcp.json`.
- SSH-пароли — открытым текстом в `ssh.json` и `datas.md` проекта.
- `keychain_get` — **только агентский инструмент чтения**, для секретов самого приложения не используется.
- Принятие лицензии — обычный JSON `license-accepted.json`, версия 1, без anti-tamper.
- CSP — `null`.
- Интеграции с Keychain для секретов приложения нет; на Windows аналог (Credential Manager/DPAPI) пока не выбран.

### 0.6. Чего нет сегодня (не implemented)

Чтобы не читать план как отчёт о сделанном:

- нет Windows-сборки, CI, подписи, notarization, updater;
- нет single-instance и deep-link;
- нет `bundle.windows`, выбора режима WebView2, настройки UDF;
- нет интеграции с Recycle Bin;
- нет платформенного слоя и `dirs`;
- нет Windows-замен для macOS-инструментов (см. §8);
- нет миграции данных macOS → Windows;
- нет Windows-тестов и матрицы устройств;
- CSP не выставлен.

### 0.7. Три слоя «корзины» — не путать

1. **Корзина приложения** — `Settings/.trash/`, 30 дней, только файлы/задачи/заметки.
2. **Удаление проекта** — **безвозвратное**, без Trash и восстановления; каскад по чатам/расписаниям/задачам.
3. **Инструмент `trash_paths`** — реальная системная корзина macOS (`~/.Trash`) + собственный ledger; `restore_trashed` возвращает оттуда.

Интеграции с Windows Recycle Bin пока нет. Это отдельное решение (§14).

---

## 1. Целевые ОС и архитектуры

- **Windows 11** — единственная целевая ОС. Практический ориентир: 24H2 и **25H2**; основная тестовая — 25H2.
- Windows 10/8/7 не поддерживаем.
- Приоритет: `x86_64-pc-windows-msvc`.
- Открытый вопрос: нужен ли `aarch64-pc-windows-msvc` (Surface/ARM). Если да — отдельная сборка, тесты и **отдельные ONNX-бинарники** под ARM64.
- Проверять: Home/Pro/Enterprise, доменные политики, RDP, отсутствие интернета, non-admin пользователь, AppLocker.

## 2. WebView2 — главный риск

**Сегодня конфигурации WebView2 нет.** В `tauri.conf.json` нет `bundle.windows`, не выбран `webviewInstallMode`, не задан UDF. Всё ниже — проект решения, а не текущее поведение.

### 2.1. Проверка наличия

На Windows 11 Evergreen WebView2 обычно предустановлен, но его можно удалить, сломать или отключить политикой; поэтому проверку и fallback сохраняем.

Проверять несколько источников, а не один:

| Что | Где смотреть |
|---|---|
| Evergreen Runtime (x64) | `HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}` → `pv` |
| Evergreen Runtime (per-user) | `HKCU\Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}` → `pv` |
| Файл runtime | `C:\Program Files (x86)\Microsoft\EdgeWebView\Application\<version>\msedgewebview2.exe` |
| Официальный API | `GetAvailableCoreWebView2BrowserVersionString` (WebView2 SDK) |
| Версия/архитектура | сравнить `pv` с минимально допустимой; проверить разрядность процесса |

Правила:
- Если runtime найден и версия >= минимальной — используем системный.
- Если найден, но старый/битый — не ломаемся, переходим на встроенный.
- Если не найден — встроенный runtime, без обязательного интернета.
- Результат проверки писать в лог и показывать в «О приложении»/диагностике.

### 2.2. Варианты поставки

Tauri 2 (`bundle.windows.webviewInstallMode`):

| Режим | Что делает | Плюсы | Минусы |
|---|---|---|---|
| `downloadBootstrapper` | качает WebView2 при установке | маленький установщик | нужен интернет, зависит от Microsoft |
| `embedBootstrapper` | несёт бутстраппер | интернет только на установке | всё ещё нужен доступ к сети |
| `offlineInstaller` | несёт offline-установщик WebView2 | работает без интернета, ставит в систему | +размер, нужны права/политики |
| `fixedRuntime` | несёт фиксированную версию WebView2 с приложением | полностью self-contained, без админа | +размер, обновления безопасности везём сами |

Рекомендуемая схема:
1. Установщик проверяет системный WebView2.
2. Если есть — использует его.
3. Если нет — **fixedRuntime** рядом с приложением (приоритет) либо `offlineInstaller` (если политика требует системной установки).
4. Явно документировать, какой режим выбран для каждой сборки.

### 2.3. Fixed Runtime

- Хранить в папке приложения, например `runtime\WebView2\`.
- Указать Tauri путь к fixed runtime; проверить, что `msedgewebview2.exe` и зависимости на месте.
- Переменные для диагностики: `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER`, `WEBVIEW2_USER_DATA_FOLDER`.
- Не смешивать системный и fixed runtime в одном профиле.
- Обновлять fixed runtime вместе с релизами (CVE WebView2/Chromium).
- Проверить лицензию Microsoft на распространение fixed runtime.

### 2.4. User Data Folder (UDF)

- Целевой путь по умолчанию: `%LOCALAPPDATA%\Project-S\WebView2` (не в Program Files, не в roaming). Сейчас UDF не настраивается.
- Права пользователя; очистка при удалении — по выбору (не удалять данные чатов без подтверждения).
- Несколько экземпляров приложения не должны делить один UDF; single-instance обязателен.
- Антивирус/OneDrive могут лочить файлы профиля — предусмотреть диагностику и восстановление.
- Ошибки WebView2 (GPU, sandbox, профиль) показывать понятным экраном с кнопкой «Перезапустить» и ссылкой на логи.

### 2.5. Что проверить в WebView2

- Рендеринг CSS/шрифтов/скроллов, `backdrop-filter`, `position: sticky`, SVG.
- `localStorage`. **IndexedDB приложение не использует.** Актуальные ключи:
  - `project-s.tasks.view.<projectId>` — режим Kanban/List;
  - `project-s:lastProject:<encoded workingDir>` — последний проект.
- **Origin:** на macOS это `tauri://localhost`, в WebView2 — `http://tauri.localhost`. `localStorage` привязан к origin, поэтому переносить его между платформами нельзя «как есть»; проверить, что ключи читаются в Windows-сборке.
- Drag-and-drop, clipboard, file input, PDF/iframe.
- Стриминг SSE и долгие запросы.
- DevTools в dev-сборке; в release отключить.
- Поведение при `prefers-reduced-motion`, high contrast, масштабе 125/150/200%.

## 3. Установщик и жизненный цикл

Сегодня: `bundle.targets` — только macOS, `bundle.windows` нет, single-instance/deep-link/updater/подписи нет, `scripts/install-app.sh` — bash + `ditto`/`xattr`/`pkill`. Всё ниже — то, что предстоит.

- Формат: NSIS (per-user, проще) или WiX/MSI (per-machine, для компаний). Открытый вопрос — что основное.
- Права: приложение не должно требовать администратора; писать только в `%LOCALAPPDATA%`/`%APPDATA%`/Documents.
- Установка/обновление/удаление: сохранять пользовательские данные, не удалять рабочие папки проектов.
- Ярлыки: меню «Пуск», рабочий стол (опционально), закрепление.
- Ассоциации файлов (`.md`, изображения, PDF?) — только если реально нужны; не захватывать чужие.
- Deep links / second launch: single-instance mutex + передача аргументов в уже запущенный экземпляр.
- Автозапуск: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` или Task Scheduler; учитывать отключение через Диспетчер задач.
- Обновления: Tauri updater или собственный канал; подпись обновлений; откат; миграции схемы БД.
- Uninstall: очистка автозапуска, ярлыков, ассоциаций; данные — по выбору пользователя.

## 4. Пути и файловая система

- **Платформенного резолвера нет:** `HOME` используется напрямую, `dirs` не подключён. Рабочая папка по умолчанию — хардкод `$HOME/Documents/PROJECT-S`.
- На Windows целевой дефолт — `%USERPROFILE%\Documents\PROJECT-S`, но получить его надо через `dirs::document_dir()` (или явный выбор пользователя), а не через конкатенацию `HOME`.
- Системный конфиг: `%APPDATA%\app.project-s.ru`; там только `working_dir.txt` и `license-accepted.json`.
- Остальные данные — под `<workingDir>/Settings/...` и `<workingDir>/Harness/...` (см. §0.4). Не переносить их в `%APPDATA%` без отдельного решения: это изменит текущую модель «всё рядом с рабочей папкой».
- Кэш/WebView2 — `%LOCALAPPDATA%\Project-S` (если так решим для UDF).
- Разделители: `\`; внутри БД/конфигов нормализовать к `/` или хранить нативно, но не смешивать.
- `canonicalize` на Windows возвращает `\\?\C:\...` — это ломает сравнение путей, вывод в UI и часть SQL-фильтров. Нужна обёртка, которая снимает verbatim-префикс там, где он не нужен.
- В БД уже лежат абсолютные пути (`projects.path`, `trash.*`, `versions.*`, `message_attachments.saved_path`, `processes.cwd/log_path`, индекс). При миграции их нужно переписать; при старте на новой платформе — валидировать.
- Регистр: NTFS регистронезависим по умолчанию; не полагаться на это (WSL/сетевые шары).
- Длинные пути: включить long paths manifest + `LongPathsEnabled`; использовать `\\?\` только осознанно.
- Зарезервированные имена: `CON`, `PRN`, `AUX`, `NUL`, `COM1..9`, `LPT1..9`; хвостовые точки/пробелы; `<>:"/\|?*` — санитайзить имена заметок, бэкапов, экспортов.
- Симлинки/junction points/reparse points: проверять канонизацию и запрет выхода за корень (как в `path_policy`), не следовать вслепую. `path_policy` сейчас опирается на Unix-семантику и `HOME`.
- Сетевые шары/UNC, съёмные диски, OneDrive placeholder-файлы: ошибки чтения/записи, блокировки, «файл недоступен».
- Антивирус и индексатор: временные локи, ложные срабатывания, задержки; повторять операции с backoff.
- ACL/read-only: понятные ошибки, не падать.
- CRLF/LF: заметки, экспорт, `.md`; не менять переводы строк без причины; UTF-8 без BOM по умолчанию.
- Имена с кириллицей/эмодзи: NFC/NFD, сортировка, поиск, ZIP.
- **Ресурсы скиллов:** Tauri на Windows кладёт ресурсы в `exe_dir/resources/skills/...`, а код ищет macOS-путь `Contents/Resources` или плоский вариант. Это отдельный баг, который проявится сразу после первой сборки.

## 5. Процессы, shell и инструменты

### 5.1. Shell

- Нет `/bin/sh`/bash. Использовать PowerShell (`pwsh.exe`/`powershell.exe`) или `cmd.exe` осознанно.
- Аргументы экранировать по правилам Windows (`CommandLineToArgvW`), не склеивать строку вручную.
- `PATHEXT`, `.exe/.cmd/.bat/.ps1`; запуск `.ps1` — с `-ExecutionPolicy Bypass` только если это явно разрешено.
- Скрывать консольные окна: `CREATE_NO_WINDOW` (0x08000000), иначе будут мигать окна.
- Кодовая страница: вывод `cmd` может быть CP866/CP1251; принудительно `chcp 65001`/UTF-8 или декодировать явно.
- Process tree: Job Objects, иначе дочерние процессы переживут родителя; kill tree.
- Сигналов POSIX нет: `Child::kill()` = TerminateProcess; graceful shutdown — через stdin/CTRL_BREAK, если поддерживается.
- Текущий `processes.rs` строит process group и проверяет живость через `kill -0`/PGID. Это надо заменить на Job Object + `GetExitCodeProcess`, а не «портировать один в один».

### 5.2. SSH/SCP

- OpenSSH Client — опциональный компонент Windows (`OpenSSH.Client~~~~0.0.1.0`), может отсутствовать.
- Пути: `%WINDIR%\System32\OpenSSH\ssh.exe`, `scp.exe`; альтернативы — Git for Windows, PuTTY/plink.
- Ключи: `%USERPROFILE%\.ssh`; ACL должны быть restrictive (не «Everyone»), иначе ssh откажется.
- Текущий `ssh_tools.rs` создаёт askpass-файл `0600` и скрипт `0700`, запускает `bash -lc`. На Windows: ACL через `icacls`/Win32 security API, askpass — отдельный `.exe`/`.cmd` или `SSH_ASKPASS` с осторожностью.
- `ssh-agent`/Pageant; `known_hosts`; пароли не хранить в конфиге — сейчас хранятся открытым текстом, это отдельный долг.
- Пробелы/кириллица в путях; кавычки; `-o StrictHostKeyChecking`.
- Проверять наличие ssh при старте и в диагностике; понятная ошибка, если нет.

### 5.3. Git

- `git.exe` может отсутствовать; не предполагать.
- Git for Windows: `core.autocrlf`, `core.longpaths`, credential manager.
- Безопасные директории (`safe.directory`), владелец файлов.
- Не тащить git в установщик без необходимости; предложить установить.

### 5.4. Браузер/CDP

- Сейчас `cdp/mod.rs` содержит macOS-пути Chrome и `PermissionsExt`.
- Chrome: `C:\Program Files\Google\Chrome\Application\chrome.exe`, `%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe`.
- Edge: `C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe`.
- Headless: `--headless=new`, `--remote-debugging-port`, отдельный `--user-data-dir`.
- Профиль: не использовать личный профиль пользователя; чистить после сессии.
- Антивирус/политики могут блокировать запуск; ошибки показывать.
- Браузер — опциональный инструмент; без него чат и харнес должны работать.

### 5.5. Скриншоты

- Сейчас на macOS — `screencapture`. На Windows: GDI/BitBlt для простого захвата; Windows Graphics Capture для окон/мониторов.
- DPI awareness (Per-Monitor V2), multi-monitor, отрицательные координаты.
- UAC secure desktop не захватывается; HDR/цветовые профили.
- Портативный fallback не полагаться на `snippingtool`; лучше своя реализация.

### 5.6. Clipboard

- Сейчас текст — `pbcopy/pbpaste`, картинки — `osascript` (PNGf/TIFF) + `sips`.
- На Windows: Win32 clipboard, `CF_DIB`/`CF_PNG`, `CF_HDROP` для файлов; конвертация через `image`.
- История буфера и RDP; большие изображения; очистка.
- Для файлов из буфера — проверять пути и копировать, а не перемещать.

### 5.7. OCR/Vision

- Apple Vision (Swift) недоступен. Варианты: Windows.Media.Ocr (WinRT, языковые пакеты), Tesseract (вендорить + `tessdata`), vision-провайдер LLM.
- Языки: русский/английский; проверять наличие языкового пакета.
- Если OCR недоступен — не ломать сценарий, предложить vision-провайдера.

### 5.8. Open with / ассоциации

- Сейчас `open_with` идёт через `NSWorkspace`. На Windows: `ShellExecuteEx`/`start`, `IApplicationAssociationRegistration`; UWP-приложения.
- «Открыть в…»: запоминать выбранное приложение, но не ломать системные ассоциации.
- .md/PDF/изображения/архивы; пути с пробелами.
- `fs_reveal_in_finder` → `explorer.exe /select,` (и аккуратно с длинными путями).

### 5.9. Уведомления, tray, autostart

- Сейчас `notify` — `osascript display notification`.
- Toast: AppUserModelID + ярлык в меню «Пуск»; иначе уведомления не показываются.
- Focus Assist/«Не беспокоить»; клик по уведомлению и действия. В macOS-реализации клик/действия есть; на Windows нужен явный fallback, если action/click не поддерживаются.
- Tray: иконка, меню, левый/правый клик, тёмная тема, DPI.
- Autostart: реестр/Task Scheduler; проверять, не отключил ли пользователь.

## 6. Агентский харнес на Windows

- Shell-инструмент: PowerShell/cmd, кодировки, quoting, Job Objects, таймауты, kill tree.
- Файловые инструменты: пути, junctions, ADS, long paths, локи, read-only, скрытые/системные файлы.
- Process-инструменты: фоновые процессы, перенаправление вывода, кодировка, окна.
- Browser/CDP: пути Chrome/Edge, sandbox, профиль.
- SSH/SCP: наличие ssh, ACL ключей, askpass.
- Screenshot/OCR/clipboard: см. выше.
- Scheduler: in-app планировщик + сон/пробуждение; Windows Task Scheduler — опционально; DST/таймзоны. Уведомления планировщика и непрочитанные — проверить на Windows отдельно.
- File watching: `ReadDirectoryChangesW`, буфер, OneDrive, debounce.
- Сеть: прокси, корпоративный MITM, TLS-хранилище Windows, `NO_PROXY`, localhost/IPv6, firewall-запрос.
- ONNX Runtime: `ort` **скачивает бинарники на этапе сборки**. Для Windows нужны `onnxruntime.dll` под x64/ARM64, MSVC runtime, офлайн-раскладка и учёт размера в установщике. Модель ~330 МБ уже в бандле.
- MCP: stdio/http/sse, тумблеры, статусы, таймауты. На Windows — резолв `.cmd`/`.exe`, `PATHEXT`, корректный kill дерева процессов (Job Objects), иначе stdio-серверы останутся висеть.
- Скиллы: 38 встроенных, синхронизация и очистка. На Windows сломается путь ресурсов (см. §4) и интерпретаторы: `python3` → `python`/`py`, `bash` отсутствует.

## 7. Текущая матрица новых фич

| Фича | macOS сегодня | Что нужно на Windows |
|---|---|---|
| «Мои задачи» | Скрытая служебная папка `Projects/.my-tasks`, всегда первая, без архива/удаления, только задачи (Files скрыты), статичное приветствие ассистента, флаг схемы v40 | Скрытый атрибут папки, joins путей, стабильная сортировка «первая», запрет delete/archive в UI и API, скрытие Files |
| Лицензионный гейт | Перед онбордингом, только для новых установок, `license-accepted.json` в app config dir | Тот же файл в `%APPDATA%\app.project-s.ru`; проверить первый запуск/обновление, отсутствие anti-tamper не менять |
| Масштаб UI | 100/110/130/175 через `getCurrentWebview().setZoom`, ACL `core:webview:allow-set-webview-zoom` | Проверить взаимодействие с OS DPI 125/150/200: двойное масштабирование, размытие, размеры окна |
| Контекст модели | `modelContextWindows`, `modelContextMax`, дефолт 100k/250k при неизвестной модели, уплотнение (tail 45%, reasoning tokens, лимит суммаризатора, `agent-compaction-failed`, маршрутизация событий) | Платформонезависимо; проверить токенайзер/модель на Windows, тайминги и SSE |
| Встроенная модель | EmbeddingGemma 2 ONNX ~330 МБ, `ort-backend`, `ort` качает бинарники при сборке | `onnxruntime.dll` x64/ARM64, MSVC runtime, офлайн-раскладка, размер установщика, загрузка на чистой машине |
| MCP | stdio/http/sse, тумблеры, статусы, таймауты | Резолв `.cmd`/`.exe`, `.bat`, Job Objects для дочерних процессов, кодировки |
| 38 встроенных скиллов | `Settings/skills/{builtin,user}`, `enabled.json`, `.builtin-version`, sync cleanup | Путь ресурсов `exe_dir/resources/skills`, замена `python3`/`bash`, проверка sync/cleanup после обновления |
| Kanban/List | `localStorage["project-s.tasks.view.<projectId>"]` | Другой origin WebView2; проверить чтение ключа, отсутствие IndexedDB |
| Фоновый чат, очередь, stop | Store чата, очередь, остановка | Kill дерева процессов, корректное завершение стрима, восстановление после рестарта |
| Расписание и уведомления | Уведомления, клик, непрочитанные; таймзоны | Toast + AUMID, fallback для click/actions, таймзоны/DST, поведение при сне |
| Архив/удаление проекта | Каскад по чатам/расписаниям/задачам; удаление **безвозвратное** | Повторы при file locks/AV/OneDrive, reparse points, **нет** Recycle Bin; явно предупреждать пользователя |
| Каскад чата/расписания | Пер-проектный чат, каскадное удаление | Платформонезависимо; проверить FK/транзакции на Windows SQLite |
| Single-instance/deep-link | Отсутствует | Mutex + передача аргументов; deep-link регистрация |
| Обновление | Отсутствует | Tauri updater/канал, подпись, откат, миграции v40+ |
| WebView2/установщик | Неприменимо | Режим поставки, UDF, fixed runtime, NSIS/MSI |
| Секреты | Plaintext JSON, `keychain_get` только для агента | Решить: оставить plaintext для паритета или Credential Manager/DPAPI (меняет формат и миграцию) |

## 8. Новые инструменты компьютера: macOS → Windows

Все перечисленные — инструменты харнеса. Паритет поведения, а не только наличие кнопки.

| Инструмент | macOS сегодня | Windows |
|---|---|---|
| `macos_ui` | osascript / System Events / Accessibility | UI Automation + `SendInput`; UAC/UIPI: из non-elevated нельзя управлять elevated-окнами |
| `clipboard_get_image` | osascript PNGf/TIFF + `sips` | Win32 clipboard `CF_DIB`/`CF_PNG` + `image`; DPI и альфа-канал |
| `launchd` | `launchctl` + `~/Library/LaunchAgents` | Task Scheduler (COM) или папка Startup; другая модель жизненного цикла |
| `mdfind` | `mdfind`, `kMDItem*` | Windows Search (OLE DB) или собственный индекс; **другой язык запросов**, паритет не 1:1 |
| `trash_paths` / `restore_trashed` | `~/.Trash` + ledger | Recycle Bin через `IFileOperation`/`SHFileOperation` + ledger; восстановление и метаданные другие |
| `sysinfo` | `sysinfo` + `pmset`/`route` | `sysinfo` + `GetSystemPowerStatus`/`GetBestRoute2` |
| `notify` | `osascript display notification` | Tauri toast + AUMID; action/click — fallback |
| `open` | `/usr/bin/open` | `ShellExecuteEx`/`start` |
| `archive` | `zip`/`unzip`/`tar`/`7z`, пути Homebrew | `tar.exe` + бандл 7-Zip **или** Rust `zip`/`tar`/`flate2`; решить по размеру/лицензии |
| `fs_watch` | `notify` | `ReadDirectoryChangesW`; OneDrive, AV, сетевые шары, переполнение буфера |
| `keychain_get` | `security` CLI | Credential Manager/DPAPI; формат секретов другой, паритет ограничен |
| `defaults_write` | `defaults` CLI | Реестр (`reg.exe`/Registry API); не писать в чужие ветки, откат |
| `pdf_to_image` | `pdftoppm`/`sips`/`qlmanage` | PDFium/Poppler; лицензии и размер |

Существующие macOS-only инструменты, которым тоже нужна замена:

| Область | macOS | Windows |
|---|---|---|
| Скриншот | `screencapture` | GDI/WGC, DPI-aware |
| OCR | Swift Vision | Windows.Media.Ocr / Tesseract / LLM vision |
| PDF merge/split/fill | PDFKit | PDFium/Poppler + отдельная запись/заполнение форм |
| `image_transform` | `sips` | `image` crate |
| Clipboard (текст) | `pbcopy`/`pbpaste` | Win32 clipboard |
| `open_with` | `NSWorkspace` | `ShellExecuteEx`, ассоциации |
| `fs_reveal_in_finder` | Finder | `explorer.exe /select,` |
| `bash` | bash | PowerShell/cmd, без bash |
| SSH askpass | `bash -lc` + 0600/0700 | ACL, `.cmd`/`.exe` askpass |
| Chrome/CDP | macOS-пути Chrome | `Program Files`/`LOCALAPPDATA` пути Chrome/Edge |
| PDF-шрифты | системные macOS-шрифты | Segoe UI/шрифты PDFium, проверка кириллицы |

## 9. Платформенный слой: что сделать до первой Windows-сборки

Это P0. Без этого нет смысла оценивать фичи.

| # | Задача | Критерий готовности |
|---|---|---|
| P0-1 | Убрать Unix-импорты из production-кода (`processes.rs`, `skills/exec.rs`, `cdp/mod.rs`, `ssh_tools.rs`) | `cargo check --target x86_64-pc-windows-msvc` проходит; Unix-код за `#[cfg(unix)]`, Windows-реализация рядом |
| P0-2 | Процессы: Job Objects, kill tree, `GetExitCodeProcess`, без `process_group`/`kill -0`/PGID | Дочерние процессы не переживают родителя; stop/queue работают |
| P0-3 | Shell: убрать `sh`/`bash -lc`; PowerShell/cmd с корректным quoting и `CREATE_NO_WINDOW` | Ни одна фича не вызывает bash; вывод в UTF-8 |
| P0-4 | Дом и пути: `HOME` → `dirs` (или явный platform resolver), убрать `/Users`, `/Applications`, Homebrew, `/usr/bin` | Нет хардкода macOS-путей в production; дефолт рабочей папки корректен |
| P0-5 | Канонизация и БД: обёртка над `canonicalize` (снятие `\\?\`), валидация абсолютных путей, reparse points | Пути в UI/БД консистентны; нет `\\?\` в тексте ошибок |
| P0-6 | Ресурсы: путь скиллов/модели для Windows (`exe_dir/resources/...`), без `Contents/Resources` | 38 скиллов находятся после установки, sync/cleanup работает |
| P0-7 | `bundle.windows`, `bundle.targets`, иконки, titlebar, single-instance | Установщик собирается, второй запуск не плодит окна |

P1 сразу после: deep-link, updater, подпись, UDF, фиксированный WebView2, DnD, tray, автозапуск, миграция данных.

## 10. Данные, БД и миграция

- SQLite: WAL; блокировки; антивирус; FTS5 и sqlite-vec (rusqlite bundled). Схема **v40**.
- Пути внутри БД абсолютные и macOS-специфичные; при переносе данных нужна нормализация/ремонт (см. §4).
- Обратная совместимость: миграции v1..v40; не откатывать `user_version`; новые Windows-миграции не должны ломать macOS.
- Экспорт/импорт: ZIP-пути — прямые слэши; UTF-8; имена файлов; CRLF.
- Бэкапы: `project-s-DD-MM-YYYY_HH-MM.zip`. **Исключения:** `Settings/config`, `Settings/models`, `Settings/skills`, `.trash`, `.versions`. То есть бэкап не спасает ключи, модель, скиллы и удалённое — это надо явно писать в UI.
- Проверять, что ZIP читается Windows Explorer и 7-Zip.
- Trash/undo: три разных механизма (см. §0.7). Не смешивать с Recycle Bin без отдельного решения.
- Настройки: `Settings/config/*.json`; при обновлении не терять. Сейчас пути к ним вычисляются от рабочей папки, а не от app config dir.
- Удаление проекта безвозвратно; ретраи при locks/AV/OneDrive; reparse points; восстановления нет.
- Миграция с macOS: отдельный сценарий (экспорт → импорт или перенос папки + переписать абсолютные пути), тесты на копии данных.

## 11. UI/UX-паритет

- Шрифты: UI рассчитан на системный **SF Pro**; отдельных Windows-шрифтов нет. SF Pro нельзя распространять; использовать Segoe UI Variable/Segoe UI/Inter; проверить кириллицу и метрики.
- Заголовок окна: сейчас macOS-светофоры/титулбар; нативного Windows-титулбара и Snap Layouts нет. Решить: стандартный титулбар или custom с учётом Snap Layouts.
- Масштаб: 100/110/130/175 через `setZoom`; на Windows это складывается с OS DPI 125/150/200. Проверить, что нет двойного масштабирования и что окно не вылезает за экран.
- Меню: системное меню/Alt; контекстные меню; горячие клавиши Cmd→Ctrl.
- Скроллбары, hover, focus-ring, high contrast, reduced motion.
- IME/кириллица, ввод в textarea/редакторе, Ctrl+Z/Y, выделение.
- DPI 125/150/200%, multi-monitor, разные масштабы.
- DnD из Explorer: OLE/CF_HDROP, копирование, длинные пути, UIPI (из elevated в non-elevated не работает).
- PDF/iframe, изображения, markdown, code blocks — проверить в WebView2.
- Доступность: UIA/Narrator, клавиатурная навигация, контраст.
- Локализация: русский UI, форматы дат/чисел, часовой пояс.
- Тёмная тема: сейчас светлая macOS-тема; на Windows не включать автоматически тёмную без отдельного решения.

## 12. Сборка, подпись, CI

Сегодня ничего из этого нет.

- Toolchain: Rust MSVC + Visual Studio Build Tools; WebView2 SDK; Node/npm.
- Target: `x86_64-pc-windows-msvc`; ARM64 — отдельно.
- `ort` скачивает бинарники при сборке: нужен доступ в сеть на CI или кэш/офлайн-раскладка; для Windows — x64/ARM64 DLL и MSVC runtime.
- CI: GitHub Actions `windows-latest` (или self-hosted), кэш cargo/npm, сборка NSIS/MSI, тесты.
- Кросс-компиляция с macOS не рекомендуется; собирать на Windows.
- Подпись: Authenticode для exe/установщика, timestamp; EV-сертификат для SmartScreen-репутации (или время/репутация).
- SmartScreen/Defender/antivirus: ложные срабатывания, подпись, отправка в Microsoft.
- Размер: приложение ~390 МБ (включая модель ~330 МБ), DMG ~300 МБ. Установщик ожидаемо ~300–450 МБ; fixed runtime добавит ещё. Точные цифры — после первой сборки, бюджет зафиксировать.
- Автообновление: подпись манифеста, HTTPS, rollback, миграции БД.
- Артефакты: `.exe` (NSIS), `.msi`, portable ZIP (если нужен), checksums, release notes.
- `scripts/install-app.sh` (bash + `ditto`/`xattr`/`pkill`) на Windows неприменим — нужен отдельный установщик/скрипт.

## 13. Тестовая матрица

- Windows 11 **25H2** (основная) + 24H2; x64. ARM64 — если поддерживаем.
- WebView2: есть/нет/старый/битый; fixedRuntime; offline.
- UI zoom 100/110/130/175 × OS DPI 125/150/200; 2 монитора, RDP, разные масштабы.
- Права: обычный пользователь, админ, доменные политики, AppLocker.
- Пути: кириллица, пробелы, длинные пути, OneDrive, сетевой диск, read-only, reparse points.
- Модель: первый запуск без сети, провижининг ~330 МБ, `onnxruntime.dll`, повторный запуск, повреждённый файл.
- Скиллы: 38 встроенных, sync/cleanup после обновления, `python`/`py`, отсутствие bash.
- Сеть: offline, proxy, MITM, firewall, медленный DNS.
- Антивирус: Defender, сторонний; исключения; блокировки файлов при удалении проекта.
- Уведомления: toast, клик, действия, Focus Assist, непрочитанные, сон/пробуждение.
- Новые инструменты: `macos_ui` (UIA/UIPI), clipboard image, Recycle Bin + ledger, Windows Search, `archive`, `fs_watch`, `defaults_write`, `pdf_to_image`, `sysinfo`, `open`.
- Сценарии: запуск, лицензия, онбординг, проект, «Мои задачи», Kanban/List, заметки/файлы, чат/агент, очередь/stop, расписание, настройки, бэкап/восстановление, архив, безвозвратное удаление, обновление, удаление приложения.
- Single-instance/deep-link: второй запуск, аргументы, фокус.
- Автотесты: Rust/JS тесты в CI; smoke через WebDriver/tauri-driver, если поддерживается; иначе ручной чек-лист.
- Логи: `Settings/logs` (журнал отключён) + `Harness/tmp/processes`; Event Viewer/WER; support bundle.

## 14. Открытые вопросы

Закрыто / текущая реальность:

- [x] Минимальная версия Windows: **только Windows 11**; Windows 10 не поддерживаем.
- [x] Рабочая папка: сейчас хардкод `$HOME/Documents/PROJECT-S`; `dirs` не используется. На Windows целевой дефолт — `%USERPROFILE%\Documents\PROJECT-S` через `dirs::document_dir()`.
- [x] Системная папка конфигов: `%APPDATA%\app.project-s.ru`; там только `working_dir.txt` и `license-accepted.json`.
- [x] Trash: три слоя (корзина приложения, безвозвратное удаление проекта, `trash_paths` → `~/.Trash`); Recycle Bin не интегрирован.
- [x] WebView2: конфигурации нет; fixed runtime и UDF — решения, а не текущее состояние.
- [x] Single-instance, deep-link, updater, CI, подпись: отсутствуют.
- [x] `localStorage`: `project-s.tasks.view.<projectId>` и `project-s:lastProject:<encoded workingDir>`; IndexedDB нет.

Новые / всё ещё открытые:

- [ ] Основной установщик: NSIS per-user или MSI per-machine?
- [ ] Нужна ли ARM64-сборка (и ARM64 ONNX)?
- [ ] WebView2: fixedRuntime всегда или системный + fallback?
- [ ] Допустим ли размер установщика с моделью и runtime? Бюджет?
- [ ] Секреты: оставить plaintext JSON для паритета или перейти на Credential Manager/DPAPI? Если переходить — как мигрировать?
- [ ] Модель и DLL: класть в установщик, докачивать при первом запуске или отдельный пакет?
- [ ] Автообновление: Tauri updater или внешний канал? Какой канал (stable/beta)?
- [ ] Кодовая подпись: обычный/EV сертификат, кто владелец?
- [ ] Microsoft Store/MSIX — нужен ли канал?
- [ ] Нужна ли интеграция с Recycle Bin вместо своей корзины?
- [ ] OCR: Windows.Media.Ocr, Tesseract или только vision-провайдер?
- [ ] SSH: требовать OpenSSH Client или нести свой?
- [ ] Браузер: требовать Chrome/Edge или сделать опциональным?
- [ ] Ассоциации файлов: нужны ли вообще?
- [ ] Поддержка доменных политик/AppLocker — обязательна?
- [ ] Миграция с macOS: автоматическая или экспорт/импорт?
- [ ] Аренда/архитектура: остаётся ли aarch64-only macOS и нужна ли universal-сборка?
- [ ] CSP: оставить `null` или ввести политику (сейчас `null`)?

## 15. Этапы и оценка

Прошлая оценка 6–10 недель писалась до текущей поверхности и **не учитывала отсутствие платформенного слоя**. Реалистичный диапазон для одного опытного разработчика с готовым macOS-кодом:

| Этап | Содержание | Оценка |
|---|---|---|
| 0 | Платформенный шов, снятие пяти блокеров, первая сборка, smoke | 2–3 недели |
| 1 | Пути, `dirs`, канонизация, БД v40, миграция данных | 1–1.5 недели |
| 2 | Процессы/Job Objects, shell, SSH, CDP, core-инструменты | 2–2.5 недели |
| 3 | Паритет новых фич (см. §7) + 13 компьютерных инструментов | 2–3 недели |
| 4 | WebView2, UDF, установщик, single-instance, updater | 1–1.5 недели |
| 5 | UI: шрифты, titlebar, DPI/zoom, горячие клавиши, DnD | 0.5–1 неделя |
| 6 | QA-матрица, миграция, CI, подпись, SmartScreen | 1.5–2 недели |

**Итого: 10–14 недель** ядра + риск-буфер **30–40%** на WebView2/antivirus/подпись/oneDrive/locks. Календарно — **13–19 недель**. Это не обещание: этапы 0 и 2 зависят от того, насколько глубоко Unix-специфика проросла в бизнес-логику, а этап 3 — от того, сколько инструментов признаем обязательными для паритета, а сколько — «есть, но с ограничениями».

Что ускоряет: один разработчик, знакомый с кодовой базой; отказ от ARM64; отказ от Recycle Bin и Credential Manager на первом этапе; заранее купленный сертификат; тестовая Windows-машина с OneDrive и Defender.

Что замедляет: псевдо-паритет инструментов (mdfind/PDF), несколько установщиков, доменные политики, требование «работает без интернета с первого запуска» вместе с fixed runtime и моделью.

## 16. Чек-лист «не проебать»

P0 (до первой сборки):

- [ ] Убрать `std::os::unix::*` из production; Unix-код за `#[cfg(unix)]`.
- [ ] Job Objects + kill tree вместо process group/`kill -0`.
- [ ] Убрать `sh`/`bash -lc`; PowerShell/cmd, quoting, `CREATE_NO_WINDOW`.
- [ ] `HOME` → `dirs`/platform resolver; убрать macOS-пути.
- [ ] Канонизация `\\?\`, абсолютные пути в БД, reparse points.
- [ ] Путь ресурсов `exe_dir/resources/...`; 38 скиллов находятся.
- [ ] Модель доставлена в `src-tauri/resources/models/embeddinggemma-2` (4 файла); на Windows есть `stage-embed-model.ps1` или ручная копия.
- [ ] `bundle.windows`, `bundle.targets`, иконки, titlebar, single-instance.

P1 (паритет и поставка):

- [ ] WebView2: детект → системный → fallback встроенный; fixed runtime/UDF.
- [ ] UDF в LocalAppData; второй экземпляр не делит профиль.
- [ ] Long paths, reserved names, UNC, symlinks/junctions.
- [ ] Кодировки shell (CP866/CP1251/UTF-8).
- [ ] SSH ACL ключей и наличие ssh; askpass без bash.
- [ ] Chrome/Edge пути и профиль.
- [ ] DPI/multi-monitor + UI zoom 100/110/130/175 без двойного масштабирования.
- [ ] Toast AppUserModelID; клик/действия и fallback.
- [ ] Autostart и его отключение пользователем.
- [ ] Шрифты без SF Pro; кириллица.
- [ ] Ассоциации файлов.
- [ ] Подпись/SmartScreen.
- [ ] Офлайн-установка и офлайн-модель (`onnxruntime.dll`, MSVC runtime).
- [ ] MCP `.cmd`/`.exe`, статусы, таймауты, kill tree.
- [ ] «Мои задачи»: скрытая папка, первая, без delete/archive.
- [ ] Лицензионный гейт на новом install.
- [ ] Безвозвратное удаление проекта: ретраи, locks, AV/OneDrive, reparse points, предупреждение.
- [ ] Три слоя корзины не перепутаны; Recycle Bin — отдельное решение.
- [ ] Бэкапы: исключения (`config`, `models`, `skills`, `.trash`, `.versions`) отражены в UI.
- [ ] Новые компьютерные инструменты (§8) или явно задокументированные ограничения.
- [ ] Миграция данных и путей из macOS.
- [ ] Логи/support bundle (`Settings/logs` + `Harness/tmp/processes`).

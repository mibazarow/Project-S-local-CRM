# WINDOWS-PORT — перенос Project-S на Windows

Статус: **планирование, реализация не начата**. Цель — паритет 1:1 с текущей macOS-версией: дизайн, функционал, поведение, данные, агентский харнес. Базовый стек сохраняем: **Tauri 2 + React 19 + TypeScript + Rust + SQLite**. Меняем только платформенный слой и упаковку.

> Правило: сначала пытаемся использовать системный WebView2. Если его нет, он старый или сломан — приложение обязано работать на своём встроенном runtime. Никаких «скачайте WebView2 вручную» без запасного пути.

---

## 1. Целевые ОС и архитектуры

- Windows 11 (актуальные поддерживаемые сборки). Windows 10 **не поддерживаем**.
- Приоритет: `x86_64-pc-windows-msvc`.
- Открытый вопрос: нужен ли `aarch64-pc-windows-msvc` (Surface/ARM). Если да — отдельная сборка, тесты и ONNX-бинарники под ARM64.
- Windows 10/7/8 не поддерживаем: целевая ОС — только Windows 11.
- Проверять: Home/Pro/Enterprise, доменные политики, RDP, отсутствие интернета, non-admin пользователь.

## 2. WebView2 — главный риск

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

- Путь по умолчанию: `%LOCALAPPDATA%\Project-S\WebView2` (не в Program Files, не в roaming).
- Права пользователя; очистка при удалении — по выбору (не удалять данные чатов без подтверждения).
- Несколько экземпляров приложения не должны делить один UDF; single-instance обязателен.
- Антивирус/OneDrive могут лочить файлы профиля — предусмотреть диагностику и восстановление.
- Ошибки WebView2 (GPU, sandbox, профиль) показывать понятным экраном с кнопкой «Перезапустить» и ссылкой на логи.

### 2.5. Что проверить в WebView2

- Рендеринг CSS/шрифтов/скроллов, `backdrop-filter`, `position: sticky`, SVG.
- `localStorage`/IndexedDB (сейчас там `lastProject` и порядок карточек «Сегодня»).
- Drag-and-drop, clipboard, file input, PDF/iframe.
- Стриминг SSE и долгие запросы.
- DevTools в dev-сборке; в release отключить.
- Поведение при `prefers-reduced-motion`, high contrast, масштабе 125/150/200%.

## 3. Установщик и жизненный цикл

- Формат: NSIS (per-user, проще) или WiX/MSI (per-machine, для компаний). Открытый вопрос — что основное.
- Права: приложение не должно требовать администратора; писать только в `%LOCALAPPDATA%`/`%APPDATA%`/Documents.
- Установка/обновление/удаление: сохранять пользовательские данные, не удалять рабочие папки проектов.
- Ярлыки: меню «Пуск», рабочий стол (опционально), закрепление.
- Ассоциации файлов (`.md`, изображения, PDF?) — только если реально нужны; не захватывать чужие.
- Deep links / second launch: single-instance mutex + передача аргументов в уже запущенный экземпляр.
- Автозапуск: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` или Task Scheduler; учитывать отключение через Диспетчер задач.
- Обновления: Tauri updater или собственный канал; подпись обновлений; откат.
- Uninstall: очистка автозапуска, ярлыков, ассоциаций; данные — по выбору пользователя.

## 4. Пути и файловая система

- Рабочая папка приложения: **не** хардкодить `/Users/...`. Использовать `dirs::document_dir()` → `%USERPROFILE%\Documents\PROJECT-S` (или выбор пользователя).
- Данные: `%LOCALAPPDATA%\Project-S` (кэш, WebView2, логи), `%APPDATA%\Project-S` (конфиги), проекты — в Documents.
- Разделители: `\`; внутри БД/конфигов нормализовать к `/` или хранить нативно, но не смешивать.
- Регистр: NTFS регистронезависим по умолчанию; не полагаться на это (WSL/сетевые шары).
- Длинные пути: включить long paths manifest + `LongPathsEnabled`; использовать `\\?\` только осознанно.
- Зарезервированные имена: `CON`, `PRN`, `AUX`, `NUL`, `COM1..9`, `LPT1..9`; хвостовые точки/пробелы; `<>:"/\|?*` — санитайзить имена заметок, бэкапов, экспортов.
- Симлинки/junction points/reparse points: проверять канонизацию и запрет выхода за корень (как в `path_policy`), не следовать вслепую.
- Сетевые шары/UNC, съёмные диски, OneDrive placeholder-файлы: ошибки чтения/записи, блокировки, «файл недоступен».
- Антивирус и индексатор: временные локи, ложные срабатывания, задержки; повторять операции с backoff.
- ACL/read-only: понятные ошибки, не падать.
- CRLF/LF: заметки, экспорт, `.md`; не менять переводы строк без причины; UTF-8 без BOM по умолчанию.
- Имена с кириллицей/эмодзи: NFC/NFD, сортировка, поиск, ZIP.

## 5. Процессы, shell и инструменты

### 5.1. Shell

- Нет `/bin/sh`/bash. Использовать PowerShell (`pwsh.exe`/`powershell.exe`) или `cmd.exe` осознанно.
- Аргументы экранировать по правилам Windows (`CommandLineToArgvW`), не склеивать строку вручную.
- `PATHEXT`, `.exe/.cmd/.bat/.ps1`; запуск `.ps1` — с `-ExecutionPolicy Bypass` только если это явно разрешено.
- Скрывать консольные окна: `CREATE_NO_WINDOW` (0x08000000), иначе будут мигать окна.
- Кодовая страница: вывод `cmd` может быть CP866/CP1251; принудительно `chcp 65001`/UTF-8 или декодировать явно.
- Process tree: Job Objects, иначе дочерние процессы переживут родителя; kill tree.
- Сигналов POSIX нет: `Child::kill()` = TerminateProcess; graceful shutdown — через stdin/CTRL_BREAK, если поддерживается.

### 5.2. SSH/SCP

- OpenSSH Client — опциональный компонент Windows (`OpenSSH.Client~~~~0.0.1.0`), может отсутствовать.
- Пути: `%WINDIR%\System32\OpenSSH\ssh.exe`, `scp.exe`; альтернативы — Git for Windows, PuTTY/plink.
- Ключи: `%USERPROFILE%\.ssh`; ACL должны быть restrictive (не «Everyone»), иначе ssh откажется.
- `ssh-agent`/Pageant; `known_hosts`; askpass; пароли не хранить в конфиге.
- Пробелы/кириллица в путях; кавычки; `-o StrictHostKeyChecking`.
- Проверять наличие ssh при старте и в диагностике; понятная ошибка, если нет.

### 5.3. Git

- `git.exe` может отсутствовать; не предполагать.
- Git for Windows: `core.autocrlf`, `core.longpaths`, credential manager.
- Безопасные директории (`safe.directory`), владелец файлов.
- Не тащить git в установщик без необходимости; предложить установить.

### 5.4. Браузер/CDP

- Chrome: `C:\Program Files\Google\Chrome\Application\chrome.exe`, `%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe`.
- Edge: `C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe`.
- Headless: `--headless=new`, `--remote-debugging-port`, отдельный `--user-data-dir`.
- Профиль: не использовать личный профиль пользователя; чистить после сессии.
- Антивирус/политики могут блокировать запуск; ошибки показывать.
- Браузер — опциональный инструмент; без него чат и харнес должны работать.

### 5.5. Скриншоты

- GDI/BitBlt для простого захвата; Windows Graphics Capture для окон/мониторов.
- DPI awareness (Per-Monitor V2), multi-monitor, отрицательные координаты.
- UAC secure desktop не захватывается; HDR/цветовые профили.
- Портативный fallback: `snippingtool`? — не полагаться, лучше своя реализация.

### 5.6. Clipboard

- Текст/HTML/изображения/файлы (`CF_HDROP`), несколько форматов.
- История буфера и RDP; большие изображения; очистка.
- Для файлов из буфера — проверять пути и копировать, а не перемещать.

### 5.7. OCR/Vision

- Apple Vision недоступен. Варианты: Windows.Media.Ocr (WinRT, языковые пакеты), Tesseract (вендорить + `tessdata`), vision-провайдер LLM.
- Языки: русский/английский; проверять наличие языкового пакета.
- Если OCR недоступен — не ломать сценарий, предложить vision-провайдера.

### 5.8. Open with / ассоциации

- `ShellExecuteEx`/`start`, `IApplicationAssociationRegistration`; UWP-приложения.
- «Открыть в…»: запоминать выбранное приложение, но не ломать системные ассоциации.
- .md/PDF/изображения/архивы; пути с пробелами.

### 5.9. Уведомления, tray, autostart

- Toast: AppUserModelID + ярлык в меню «Пуск»; иначе уведомления не показываются.
- Focus Assist/«Не беспокоить»; клик по уведомлению.
- Tray: иконка, меню, левый/правый клик, тёмная тема, DPI.
- Autostart: реестр/Task Scheduler; проверять, не отключил ли пользователь.

## 6. Агентский харнес на Windows

- Shell-инструмент: PowerShell/cmd, кодировки, quoting, Job Objects, таймауты, kill tree.
- Файловые инструменты: пути, junctions, ADS, long paths, локи, read-only, скрытые/системные файлы.
- Process-инструменты: фоновые процессы, перенаправление вывода, кодировка, окна.
- Browser/CDP: пути Chrome/Edge, sandbox, профиль.
- SSH/SCP: наличие ssh, ACL ключей, askpass.
- Screenshot/OCR/clipboard: см. выше.
- Scheduler: in-app планировщик + сон/пробуждение; Windows Task Scheduler — опционально; DST/таймзоны.
- File watching: `ReadDirectoryChangesW`, буфер, OneDrive, debounce.
- Сеть: прокси, корпоративный MITM, TLS-хранилище Windows, `NO_PROXY`, localhost/IPv6, firewall-запрос.
- ONNX Runtime: `onnxruntime.dll` под x64/ARM64, VC++ Redistributable, путь к модели, размер; проверить загрузку на чистой машине.

## 7. Данные, БД и миграция

- SQLite: путь в `%LOCALAPPDATA%`/Documents; WAL; блокировки; антивирус; FTS5 (rusqlite bundled).
- Пути внутри БД: сейчас могут быть macOS-абсолютные; при переносе данных нужна нормализация/ремонт.
- Обратная совместимость схемы: миграции v1..текущая; не откатывать user_version.
- Экспорт/импорт: ZIP-пути — прямые слэши; UTF-8; имена файлов; CRLF.
- Бэкапы: `project-s-DD-MM-YYYY_HH-MM.zip`; проверять, что ZIP читается Windows Explorer и 7-Zip.
- Trash/undo: текущая корзина приложения; не смешивать с Recycle Bin без отдельного решения.
- Настройки: `app.json`, `llm.json`, `mcp.json`, skills, notes vault — перенести в `%APPDATA%`/Documents; не терять при обновлении.
- Миграция с macOS: отдельный сценарий (экспорт → импорт или перенос папки + переписать абсолютные пути), тесты на копии данных.

## 8. UI/UX-паритет

- Шрифты: SF Pro нельзя распространять; использовать Segoe UI Variable/Segoe UI/Inter; проверить кириллицу и метрики.
- Заголовок окна: нативные кнопки Windows, не «светофоры»; custom titlebar — с учётом Snap Layouts.
- Меню: системное меню/Alt; контекстные меню; горячие клавиши Cmd→Ctrl.
- Скроллбары, hover, focus-ring, high contrast, reduced motion.
- IME/кириллица, ввод в textarea/редакторе, Ctrl+Z/Y, выделение.
- DPI 125/150/200%, multi-monitor, разные масштабы.
- DnD из Explorer: OLE/CF_HDROP, копирование, длинные пути, UIPI (из elevated в non-elevated не работает).
- PDF/iframe, изображения, markdown, code blocks — проверить в WebView2.
- Доступность: UIA/Narrator, клавиатурная навигация, контраст.
- Локализация: русский UI, форматы дат/чисел, часовой пояс.

## 9. Сборка, подпись, CI

- Toolchain: Rust MSVC + Visual Studio Build Tools; WebView2 SDK; Node/npm.
- Target: `x86_64-pc-windows-msvc`; ARM64 — отдельно.
- CI: GitHub Actions `windows-latest` (или self-hosted), кэш cargo/npm, сборка NSIS/MSI, тесты.
- Кросс-компиляция с macOS не рекомендуется; собирать на Windows.
- Подпись: Authenticode для exe/установщика, timestamp; EV-сертификат для SmartScreen-репутации (или время/репутация).
- SmartScreen/Defender/antivirus: ложные срабатывания, подпись, отправка в Microsoft.
- Размер: базовый + fixedRuntime; измерить и зафиксировать бюджет.
- Автообновление: подпись манифеста, HTTPS, rollback, миграции БД.
- Артефакты: `.exe` (NSIS), `.msi`, portable ZIP (если нужен), checksums, release notes.

## 10. Тестовая матрица

- Windows 11 23H2/24H2; x64 (+ARM64, если поддерживаем). Windows 10 исключена из матрицы.
- WebView2: есть/нет/старый/битый; fixedRuntime; offline.
- Права: обычный пользователь, админ, доменные политики, AppLocker.
- Пути: кириллица, пробелы, длинные пути, OneDrive, сетевой диск, read-only.
- Сеть: offline, proxy, MITM, firewall, медленный DNS.
- Антивирус: Defender, сторонний; исключения.
- DPI/мониторы: 100/125/150/200%, 2 монитора, RDP.
- Сценарии: запуск, проект, задачи, заметки/файлы, чат/агент, настройки, бэкап/восстановление, обновление, удаление.
- Автотесты: Rust/JS тесты в CI; smoke через WebDriver/tauri-driver, если поддерживается; иначе ручной чек-лист.
- Логи: `%LOCALAPPDATA%\Project-S\logs`, Event Viewer/WER, support bundle.

## 11. Открытые вопросы (озвучены, не закрыты)

- [x] Минимальная версия Windows: **только Windows 11**; Windows 10 не поддерживаем.
- [ ] Нужна ли ARM64-сборка?
- [ ] Основной установщик: NSIS per-user или MSI per-machine?
- [ ] WebView2: fixedRuntime всегда или системный + fallback?
- [ ] Допустим ли размер установщика с runtime? Бюджет?
- [ ] Автообновление: Tauri updater или внешний канал?
- [ ] Кодовая подпись: обычный/EV сертификат, кто владелец?
- [ ] Microsoft Store/MSIX — нужен ли канал?
- [ ] Где хранить проекты по умолчанию: Documents или выбор пользователя?
- [ ] Нужна ли интеграция с Recycle Bin вместо своей корзины?
- [ ] OCR: Windows.Media.Ocr, Tesseract или только vision-провайдер?
- [ ] SSH: требовать OpenSSH Client или нести свой?
- [ ] Браузер: требовать Chrome/Edge или сделать опциональным?
- [ ] Ассоциации файлов: нужны ли вообще?
- [ ] Поддержка доменных политик/AppLocker — обязательна?
- [ ] Миграция с macOS: автоматическая или экспорт/импорт?

## 12. Этапы и оценка

1. **Спайк (2–3 дня):** Tauri-сборка на Windows, проверка WebView2 (есть/нет), fixedRuntime, один IPC-вызов, стриминг, SQLite, DnD.
2. **Платформенный слой (1–2 недели):** пути, shell, процессы, open-with, clipboard, screenshot, autostart, tray, уведомления.
3. **WebView2-поставка (3–5 дней):** детект, fallback, установщик, офлайн, логи.
4. **Агентские инструменты (1–2 недели):** shell/ssh/browser/OCR/processes, тесты.
5. **UI-паритет (1–2 недели):** шрифты, заголовок, DPI, DnD, горячие клавиши, доступность.
6. **Упаковка/подпись/CI (1 неделя):** NSIS/MSI, Authenticode, SmartScreen, автообновление.
7. **QA-матрица и миграция данных (1–2 недели).**

Итого: **6–10 недель** при одном опытном разработчике и готовом macOS-коде; риск-буфер +30% на WebView2/antivirus/подпись.

## 13. Чек-лист «не проебать»

- [ ] WebView2: детект → системный → fallback встроенный.
- [ ] UDF в LocalAppData, single-instance.
- [ ] Long paths, reserved names, UNC, symlinks/junctions.
- [ ] Кодировки shell (CP866/CP1251/UTF-8).
- [ ] Job Objects/kill tree.
- [ ] SSH ACL ключей и наличие ssh.
- [ ] Chrome/Edge пути и профиль.
- [ ] DPI/multi-monitor.
- [ ] Toast AppUserModelID.
- [ ] Autostart и его отключение пользователем.
- [ ] Шрифты без SF Pro.
- [ ] Ассоциации файлов.
- [ ] Подпись/SmartScreen.
- [ ] Офлайн-установка.
- [ ] Миграция данных и путей из macOS.
- [ ] Логи/support bundle.

# LINUX-PORT — перенос Project-S на Linux

Статус: **планирование, реализация не начата**. Цель — паритет 1:1 с macOS-версией: дизайн, функционал, поведение, данные, агентский харнес. Стек сохраняем: **Tauri 2 + React 19 + TypeScript + Rust + SQLite**. Linux — самая фрагментированная платформа: здесь важнее не «портировать код», а выбрать канал поставки и sandbox-модель.

> Правило: сначала проверяем системный WebKitGTK 4.1. Если его нет или он несовместим — приложение должно запуститься на своём runtime: Flatpak/Snap runtime или вложенный WebKitGTK (AppImage). Без «поставьте пакет сами» как единственного пути.

---

## 1. Целевые дистрибутивы и среды

- Приоритет: Ubuntu 24.04 LTS, Ubuntu 22.04 LTS, Debian 12, Fedora 40+, Linux Mint 22.
- Дополнительно: Arch (AUR), openSUSE (OBS), RHEL-совместимые.
- Архитектуры: `x86_64-unknown-linux-gnu`; открытый вопрос — `aarch64` (Raspberry Pi/ARM-ноутбуки).
- Сессии: **Wayland** (GNOME/KDE) и **X11**; протестировать обе.
- GPU: Intel, AMD, NVIDIA (проприетарный и nouveau); программный рендеринг как fallback.
- Минимальная glibc: собирать на Ubuntu 22.04 (glibc 2.35) или использовать Flatpak (рекомендуется), иначе бинарник не запустится на старых дистрибутивах.

## 2. WebView — главный риск

### 2.1. Что использует Tauri на Linux

- Tauri 2 использует **WebKitGTK 4.1** (`libwebkit2gtk-4.1.so.0`), GTK3, libsoup3, JavaScriptCore.
- Это не Chromium: другой движок, другие CSS/JS-особенности, другой PDF/медиа-стек.
- WebKitGTK не является частью Tauri и не входит в бинарник по умолчанию.

### 2.2. Проверка наличия

| Что | Как проверить |
|---|---|
| Библиотека | `ldconfig -p \| grep webkit2gtk-4.1`, `pkg-config --modversion webkit2gtk-4.1` |
| Пакет (Debian/Ubuntu) | `dpkg -l libwebkit2gtk-4.1-0` |
| Пакет (Fedora) | `rpm -q webkit2gtk4.1` |
| Версия | `pkg-config --modversion webkit2gtk-4.1`; сравнить с минимумом Tauri |
| Причина сбоя | `ldd` бинарника, `WEBKIT_DEBUG=1`, логи |

Правила:
- Если WebKitGTK 4.1 есть и версия подходит — используем системный.
- Если нет/старый — приложение не должно падать с «cannot open shared object»; нужен понятный экран и путь установки через пакетный менеджер/Flatpak.
- В `.deb`/`.rpm` зависимость объявлять явно; в Flatpak она уже в runtime; в AppImage — вкладывать (см. ниже).

### 2.3. Каналы поставки

| Канал | WebKitGTK | Плюсы | Минусы |
|---|---|---|---|
| **Flatpak** (рекомендуется) | в runtime `org.gnome.Platform` | предсказуемо, sandbox, обновления, работает на любом дистрибутиве | +размер runtime, portals, ограничения sandbox |
| Snap | в base snap | просто для пользователя | confinement, медленный старт, меньше контроля |
| `.deb` / `.rpm` | системный, зависимость | нативно, малый размер | не работает без пакета; старые версии WebKitGTK |
| AppImage | вложенный WebKitGTK | «один файл», без установки | хрупко: GLIBC, GTK, sandbox, GPU, FUSE; большой размер |
| AUR/Nix | системный/пакетный | для энтузиастов | не основной канал |

Рекомендация: **Flatpak — основной канал**, `.deb`/`.rpm` — для корпоративных/контролируемых машин, AppImage — только как experimental/portable.

### 2.4. Flatpak: что учесть

- Runtime: `org.gnome.Platform//46` (или актуальный) + `org.gnome.Sdk` для сборки.
- ID приложения: например `ru.example.project-s`; имя/иконка/metadata.
- Sandbox: по умолчанию нет доступа к домашней папке; нужны portals.
- Разрешения:
  - `--filesystem=home` (широко) или точечно `--filesystem=~/Documents/Project-S`;
  - `--filesystem=xdg-documents`, `xdg-download`, `xdg-config`;
  - `--share=network` для LLM/веб-поиска;
  - `--socket=wayland`, `--socket=fallback-x11`, `--device=dri`;
  - `--talk-name=org.freedesktop.portal.*`, `org.freedesktop.Notifications`;
  - `--talk-name=org.freedesktop.Flatpak` — только если нужен `flatpak-spawn --host` (опасно, расширяет sandbox).
- Портал файлов: `org.freedesktop.portal.FileChooser`; выбранные пользователем пути приходят как `/run/user/1000/doc/...` — их нельзя сохранять как «настоящие» пути без преобразования.
- DnD из хост-системы в sandbox: работает через portal; проверить `desktop_drop`/Tauri.
- `~/.ssh` и ключи: sandbox их не видит; либо `--filesystem=~/.ssh:ro`, либо отказ от SSH-инструмента в Flatpak.
- Браузер/CDP: системный Chrome в sandbox не запустить напрямую; `flatpak-spawn --host` или отказ.
- Обновления: через Flathub/свой репозиторий; Tauri updater внутри Flatpak не нужен/не работает как обычно.
- Flathub review: метаданные, лицензия, sandbox-разрешения, скриншоты, AppStream.

### 2.5. AppImage: что учесть

- Вкладывать WebKitGTK и зависимости через `linuxdeploy` + webkit-плагин; проверить, что все `.so` и `WebKitWebProcess` на месте.
- FUSE: на некоторых системах не установлен; предусмотреть `--appimage-extract-and-run`.
- GLIBC: собрать на старом дистрибутиве (Ubuntu 22.04) или в контейнере; иначе `GLIBC_2.xx not found`.
- GTK/glib/gio версии; конфликты с системными темами.
- Sandbox/SUID helper WebKitGTK; `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS` — только диагностика, не релиз.
- GPU/DMA-BUF: на NVIDIA/Wayland возможны чёрные окна; тестировать `WEBKIT_DISABLE_DMABUF_RENDERER=1`.
- Размер: десятки–сотни МБ; обновление целиком.

### 2.6. WebKitGTK-особенности

- Рендеринг отличается от WebView2/WKWebView: проверить CSS grid/flex, `backdrop-filter`, `position: sticky`, SVG, шрифты.
- PDF: WebKitGTK может не рендерить PDF во встроенном viewer; предусмотреть fallback (открыть системным просмотрщиком или PDF.js).
- Медиа/кодеки: не все кодеки доступны; видео в заметках/чате — проверить.
- DevTools: `WEBKIT_INSPECTOR_SERVER=127.0.0.1:9222` или настройки Tauri; в релизе отключить.
- DMA-BUF: `WEBKIT_DISABLE_DMABUF_RENDERER=1` при проблемах с GPU; `WEBKIT_DISABLE_COMPOSITING_MODE=1` при артефактах.
- Sandbox WebKit: bubblewrap; в контейнерах/Flatpak может не работать; проверить `webkit_web_context_set_sandbox_enabled`.
- Шрифты: fontconfig, Noto Sans/Inter; SF Pro недоступен.
- IME: fcitx5/ibus, кириллица; проверить ввод в textarea/редакторе.

## 3. Пути и файловая система

- XDG:
  - данные: `$XDG_DATA_HOME` (`~/.local/share/project-s`);
  - конфиг: `$XDG_CONFIG_HOME` (`~/.config/project-s`);
  - кэш: `$XDG_CACHE_HOME` (`~/.cache/project-s`);
  - состояние/логи: `$XDG_STATE_HOME` (`~/.local/state/project-s`);
  - документы: `xdg-user-dir DOCUMENTS` (`~/Documents`).
- Рабочая папка проектов: `~/Documents/PROJECT-S` (или выбор пользователя); не хардкодить macOS-пути.
- Flatpak: путь `~/.var/app/<app-id>/...`; хост-пути через portal; не хранить `/run/user/.../doc/...` как постоянные.
- Регистр: ext4/btrfs регистрозависимы; `README.md` и `readme.md` — разные файлы; не полагаться на регистронезависимость.
- `/` как разделитель; не смешивать с `\` в БД/экспорте.
- Симлинки: канонизация, запрет выхода за корень; не следовать в `path_policy` вслепую.
- Права: POSIX 600/700 для ключей/конфигов; umask; ACL; root не нужен; не писать в `/usr`, `/opt`, `/etc`.
- Имена: любые байты кроме `/` и NUL; пробелы, переводы строк, эмодзи; санитайзить только для экспорта/ZIP.
- Длина: 255 байт на компонент, PATH_MAX 4096; длинные кириллические имена — проверять.
- `noexec` на `/tmp`/`home` (в некоторых конфигурациях): не рассчитывать на запуск скриптов из временной папки.
- FUSE/AppImage: монтирование, права, `--appimage-extract-and-run`.
- Сетевые FS (NFS/SMB): блокировки SQLite, inotify, права; предупреждать.
- Trash: XDG trash spec (`~/.local/share/Trash`); текущая корзина приложения — решить, интегрировать или оставить свою.

## 4. Процессы, shell и инструменты

### 4.1. Shell

- `/bin/sh` есть, но это не bash; не использовать bash-измы без явного `bash`.
- GUI-запуск не наследует PATH из `.zshrc`/`.bashrc`; PATH может быть уже/другой. Использовать login shell (`bash -lc`) осознанно или абсолютные пути.
- Локаль/LANG: вывод и сортировка; `LC_ALL=C` для стабильного парсинга.
- Процессы: process groups/sessions, `setsid`, kill группы; SIGTERM → SIGKILL; zombie reaping.
- Flatpak: host-команды недоступны; `flatpak-spawn --host` — только с разрешением, безопасность.
- Скрипты: shebang, executable bit; не полагаться на расширение.
- Вывод: UTF-8 обычно, но не гарантировано; обрабатывать ошибки декодирования.

### 4.2. SSH/SCP

- OpenSSH обычно есть, но не гарантирован (минимальные образы, Flatpak).
- Ключи `~/.ssh`, права 700/600, иначе ssh откажется; known_hosts; ssh-agent (SSH_AUTH_SOCK).
- askpass: `SSH_ASKPASS`, `DISPLAY`/`WAYLAND_DISPLAY`; в Flatpak может не работать.
- Flatpak: либо `--filesystem=~/.ssh:ro` + ssh в runtime, либо отказ/внешний терминал.
- Пароли не хранить в конфиге; использовать agent/askpass.

### 4.3. Git

- `git` может отсутствовать; проверять и предлагать установку.
- `safe.directory`, владелец файлов, CRLF/LF, `core.autocrlf`.
- Flatpak: git в runtime/SDK или host через `flatpak-spawn`; иначе инструмент недоступен.

### 4.4. Браузер/CDP

- Пути: `/usr/bin/google-chrome`, `/usr/bin/chromium`, `/usr/bin/chromium-browser`, `/snap/bin/chromium`, `/var/lib/flatpak/...`.
- Snap Chromium confinement: профиль и CDP-порт могут быть недоступны.
- Flatpak Chrome: `flatpak run com.google.Chrome` + аргументы; порт наружу; sandbox.
- `--headless=new`, `--remote-debugging-port`, отдельный `--user-data-dir` в `$XDG_CACHE_HOME`.
- Не использовать личный профиль; чистить.
- Без браузера приложение должно работать (инструмент опционален).

### 4.5. Скриншоты

- X11: `import` (ImageMagick), `gnome-screenshot`, `scrot`; свой захват через Xlib/pipewire.
- Wayland: прямой захват запрещён; только portal `org.freedesktop.portal.Screenshot` (с подтверждением пользователя) или PipeWire; `grim` для wlroots.
- Flatpak: portal обязателен.
- Multi-monitor, fractional scaling, HDR; UAC-аналога нет.
- Если capture недоступен — понятная ошибка, не зависать.

### 4.6. Clipboard

- X11: CLIPBOARD/PRIMARY; Wayland: `wl-clipboard`/portal; Flatpak: portal.
- Текст/HTML/изображения/файлы (`text/uri-list`); большие данные.
- Буфер может очищаться при закрытии приложения (X11 ownership) — не терять данные.

### 4.7. OCR/Vision

- Apple Vision недоступен. Tesseract (`tesseract-ocr`, `tesseract-ocr-rus`) — вендорить или требовать; языковые пакеты.
- Flatpak: tesseract в runtime/bundle; tessdata.
- Vision-провайдер LLM — основной fallback.
- Если OCR нет — не ломать сценарий.

### 4.8. Open with / MIME

- `xdg-open`, `gio open`; `.desktop`, MIME (`xdg-mime query default`).
- Flatpak: portal `OpenURI`; хост-приложения.
- Не менять системные ассоциации; «Открыть в…» — запоминать выбор.
- .md/PDF/изображения/архивы.

### 4.9. Уведомления, tray, autostart

- Уведомления: D-Bus `org.freedesktop.Notifications` / `notify-send`; Flatpak portal.
- Tray: StatusNotifierItem/AppIndicator; GNOME требует расширение AppIndicator; libayatana-appindicator.
- Autostart: `~/.config/autostart/project-s.desktop`; systemd user unit — опционально; Flatpak autostart через portal/`--autostart`.
- Single instance: D-Bus name/Unix socket; второй запуск передаёт аргументы.

## 5. Агентский харнес на Linux

- Shell/PATH/locale; process groups/kill tree; timeout; zombie reaping.
- Файлы: симлинки, права, регистр, длинные имена, noexec, NFS.
- Процессы: фоновые, stdout/stderr, кодировки, Flatpak sandbox.
- Browser/CDP: системный/Flatpak/Snap браузер; sandbox.
- SSH/SCP: ключи/права/agent/Flatpak.
- Screenshot/clipboard: X11 vs Wayland/portal.
- Git: наличие/права/safe.directory.
- Scheduler: in-app + сон/пробуждение; systemd timers — опционально; DST/таймзоны.
- File watching: inotify; лимит `fs.inotify.max_user_watches`; рекурсивные вотчеры; NFS; debounce.
- Сеть: proxy env (`HTTP(S)_PROXY`, `NO_PROXY`), корпоративный CA (`SSL_CERT_FILE`/`SSL_CERT_DIR`), localhost/IPv6, firewall.
- ONNX Runtime: `libonnxruntime.so`, glibc, CPU-инструкции, размер; Flatpak bundle; модели.

## 6. Данные, БД и миграция

- SQLite: `$XDG_DATA_HOME/project-s/project-s.db`; WAL; блокировки; FTS5.
- Пути внутри БД: macOS-абсолютные; при переносе нормализовать/переписать.
- Схема: миграции v1..текущая; не откатывать `user_version`; тест на копии БД.
- Экспорт/импорт: ZIP, `/`, UTF-8, права, симлинки; не разворачивать за пределы vault.
- Бэкапы: `project-s-DD-MM-YYYY_HH-MM.zip`; проверить `unzip`, Ark, Files.
- Trash: XDG trash vs своя корзина; решить.
- Конфиги: `~/.config/project-s/{app,llm,mcp}.json`; skills/notes — в данных; Flatpak — внутри `~/.var/app/...`.
- Миграция с macOS: экспорт → импорт или перенос + переписать пути; тесты.
- Права на файлы: 600 для конфигов с ключами, 700 для каталогов; не светить ключи в логах.

## 7. UI/UX-паритет

- Шрифты: SF Pro недоступен; Inter/Noto Sans/Cantarell; проверить кириллицу, метрики, вес.
- Заголовок окна: GTK/Wayland client-side decorations; кнопки справа/слева зависят от темы; не ломать drag.
- Меню: системное меню/Alt; контекстные меню; горячие клавиши Cmd→Ctrl.
- Скроллбары/темы: GTK light/dark, high contrast; не хардкодить цвета, учитывать `prefers-color-scheme`.
- DPI/fractional scaling (125/150%), multi-monitor, Wayland vs X11.
- IME: fcitx5/ibus, кириллица, ввод в редакторе.
- DnD: XDND (X11) vs Wayland DnD; Flatpak portal; `text/uri-list`; копирование.
- PDF/iframe/видео: WebKitGTK-ограничения; fallback.
- Доступность: AT-SPI/Orca, клавиатура, контраст.
- Локализация: русский UI, даты/числа, таймзона.
- Трей: GNOME-extension; не считать его гарантированным.

## 8. Сборка, упаковка, CI

- Toolchain: Rust, Node/npm, `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libsoup-3.0-dev`, `libjavascriptcoregtk-4.1-dev`, `pkg-config`, `patchelf`.
- Target: `x86_64-unknown-linux-gnu`; ARM64 — отдельно.
- Базовая glibc: Ubuntu 22.04/24.04; Flatpak снимает проблему.
- CI: Ubuntu 22.04/24.04, Fedora container, Arch (опционально); Xvfb/Weston для smoke; кэш cargo/npm.
- Flatpak: manifest, `flatpak-builder`, runtime/SDK, AppStream, иконки, sandbox-разрешения, Flathub review.
- Snap: `snapcraft.yaml`, confinement, interfaces (network, home, desktop, notifications).
- `.deb`: `dpkg-deb`, зависимости, maintainer scripts, иконки, `.desktop`, MIME.
- `.rpm`: `rpmbuild`, Fedora/openSUSE, COPR/OBS.
- AppImage: `linuxdeploy` + webkit plugin, FUSE fallback, GLIBC baseline.
- Подпись: GPG для пакетов/репозитория, Flatpak подпись; AppImage checksum.
- Автообновление: Flatpak/Flathub, Snap, apt/dnf repo, Tauri updater для AppImage (осторожно).
- Артефакты: `.flatpak`, `.snap`, `.deb`, `.rpm`, `.AppImage`, checksums, release notes.

## 9. Тестовая матрица

- Ubuntu 22.04/24.04, Debian 12, Fedora 40+, Mint 22; Arch — smoke.
- Wayland и X11; GNOME/KDE; NVIDIA/AMD/Intel; software rendering.
- WebKitGTK: есть/нет/старый; Flatpak/Snap/deb/AppImage.
- Flatpak sandbox: home access, portals, DnD, clipboard, screenshot, notifications, autostart, tray.
- Пути: кириллица, пробелы, регистр, симлинки, длинные имена, NFS, read-only.
- Сеть: offline, proxy, MITM, IPv6, firewall.
- Права: обычный пользователь, root не нужен; noexec; umask.
- DPI/мониторы: 100/125/150/200%, 2 монитора, Wayland fractional scaling.
- Сценарии: запуск, проект, задачи, заметки/файлы, чат/агент, настройки, бэкап/восстановление, обновление, удаление.
- Автотесты: Rust/JS в CI; smoke через WebDriver (`tauri-driver`/WebKitWebDriver) под Xvfb; ручной чек-лист для Wayland/portal.
- Логи: `$XDG_STATE_HOME/project-s/logs`, journald, coredump; support bundle.

## 10. Открытые вопросы (озвучены, не закрыты)

- [ ] Минимальные дистрибутивы/glibc: Ubuntu 22.04? Debian 12?
- [ ] Основной канал: Flatpak или `.deb`?
- [ ] Нужен ли Snap?
- [ ] AppImage: поддерживаем или experimental?
- [ ] ARM64: нужен?
- [ ] Flatpak-разрешения: `home` целиком или точечно Documents?
- [ ] SSH/браузер в Flatpak: разрешать host-доступ или отключать инструменты?
- [ ] Wayland screenshot: только portal (с подтверждением) или X11 fallback?
- [ ] OCR: Tesseract (bundle) или только vision-провайдер?
- [ ] Tray: поддерживаем GNOME через extension или считаем опциональным?
- [ ] Autostart: XDG autostart или systemd user unit?
- [ ] Trash: XDG trash или своя корзина?
- [ ] Шрифты: Inter или Noto Sans как основной?
- [ ] Миграция с macOS: экспорт/импорт или перенос папки?
- [ ] Автообновление: Flatpak/Flathub или свой канал?
- [ ] Подпись пакетов/репозитория: кто владелец GPG?

## 11. Этапы и оценка

1. **Спайк (3–5 дней):** Tauri-сборка на Ubuntu 24.04, WebKitGTK есть/нет, Flatpak hello + portal, SQLite, стриминг, DnD.
2. **Платформенный слой (2–3 недели):** XDG-пути, shell/PATH, процессы/сигналы, open-with, clipboard, screenshot, notifications, autostart, single-instance.
3. **WebView/упаковка (1–2 недели):** Flatpak manifest, sandbox, `.deb`, AppImage experimental, WebKitGTK fallback/диагностика.
4. **Агентские инструменты (2–3 недели):** shell/ssh/browser/OCR/processes, Flatpak-ограничения, тесты.
5. **UI-паритет (2 недели):** шрифты, заголовок, DPI, DnD, горячие клавиши, доступность, Wayland.
6. **CI/подпись/каналы (1–2 недели):** Flatpak/Flathub, deb/rpm, GPG, обновления.
7. **QA-матрица и миграция данных (2 недели).**

Итого: **8–14 недель** при одном опытном разработчике; Flatpak-путь снижает риск, AppImage — повышает. Риск-буфер +30–40% на Wayland/portal/GPU.

## 12. Финальный чек-лист

- [ ] WebKitGTK 4.1: детект → системный → Flatpak/вложенный fallback.
- [ ] Flatpak sandbox/portals: файлы, DnD, clipboard, screenshot, notifications, autostart.
- [ ] XDG-пути и перенос macOS-путей.
- [ ] Регистр, симлинки, права, длинные имена, noexec.
- [ ] Wayland vs X11, fractional scaling, NVIDIA/DMA-BUF.
- [ ] IME/кириллица, шрифты без SF Pro.
- [ ] Shell/PATH/locale, process groups, SIGTERM/SIGKILL.
- [ ] SSH-ключи/права/agent, Flatpak-доступ.
- [ ] Браузеры Chromium/Chrome/Snap/Flatpak, CDP.
- [ ] Tesseract/vision fallback.
- [ ] Tray/AppIndicator/GNOME extension.
- [ ] Flatpak/Snap/deb/rpm/AppImage, подпись, обновления.
- [ ] Миграция данных и бэкапы.
- [ ] Логи/support bundle.

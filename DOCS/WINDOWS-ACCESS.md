# WINDOWS-ACCESS — доступ к локальной Windows VM

**Локальная лабораторная VM. Только LAN. Репозиторий приватный. Не публиковать.**
Этот файл — памятка для будущих сессий macOS-агента. Windows-агента нет: VM используется только как build/test-хост.

## 1. Подключение

| Параметр | Значение |
|---|---|
| Хост | `192.168.1.122` |
| Порт | `2222` (socat на Linux-сервере → гость `192.168.122.33:22`) |
| Пользователь | `mb` (в Windows — `vm\mb`) |
| Ключ | `~/.ssh/id_ed25519_win11` (приватный, без пароля, права `600`) |
| Fingerprint ключа | `SHA256:zzdh0o3ehuxPVrNptpHkQMh/DjK6wFaatbydPE5sr4g` |
| Host key | `SHA256:w8/2UGgjArFazxSYOsXM6kX3OS+vzjobujQmG1K/qKo` (`[192.168.1.122]:2222`, ED25519) |
| Default shell | PowerShell |
| Путь к репозиторию | `C:\Users\mb\Projects\Project-S-local-CRM` |

SSH-алиас на Mac (`~/.ssh/config`):

```
Host win11
  HostName 192.168.1.122
  Port 2222
  User mb
  IdentityFile ~/.ssh/id_ed25519_win11
  IdentitiesOnly yes
  StrictHostKeyChecking accept-new
```

Проверка:

```bash
ssh win11 'hostname; whoami'
```

Ожидаемо: `VM`, `vm\mb`.

> PowerShell 5.1: не использовать `&&`; разделять команды `;`.

## 2. Предусловия

- VM `win11` запущена.
- На Linux-сервере активен `win11-ssh-proxy.service` (сейчас в норме).
- Порт 2222 слушает только `192.168.1.122` (LAN), наружу не открыт.

Если VM выключена:

- на Mac: `~/Documents/SH_SCRIPTS/Start_Virtual_Windows.sh` (терминал держит VM; закрытие терминала гасит её);
- или на Linux-сервере: `virsh -c qemu:///system start win11`.

Если работа идёт с другой машины:

- передать файл ключа `id_ed25519_win11` по защищённому каналу (не через git);
- или сгенерировать новый ключ и добавить его публичную часть в `C:\ProgramData\ssh\administrators_authorized_keys` в госте.

## 3. Проверенное состояние (2026-10-21)

- SSH: успешно, host key принят штатно, без prompt.
- Windows hostname: `VM`.
- Репозиторий: `master`, HEAD `f0280ea` (Windows demo build), рабочее дерево чистое.
- Node: `v24.22.1`.
- Cargo: `1.99.1`.
- Модель `embeddinggemma-2` доставлена в `src-tauri/resources/models/` (в git не входит).
- Demo build 2026-10-21: успешно; NSIS `src-tauri\target\release\bundle\nsis\Project-S_1.0.2_x64-setup.exe` (283.67 MiB); копия на рабочем столе `C:\Users\mb\Desktop\Project-S_1.0.2_x64-setup.exe`; установлено per-user в `C:\Users\mb\AppData\Local\Project-S`; приложение не запускалось.
- Для линковки на Windows добавлен `vendor/esaxx-rs` (CRT patch для ONNX); не удалять.

## 4. Как собирать (только по команде пользователя)

Windows — **demo-only**, без боевых данных и production-hardening. Код на Windows не правится и не коммитится; весь код, включая `platform/windows`, пишет macOS-агент.

```bash
ssh win11 'cd C:\Users\mb\Projects\Project-S-local-CRM; git pull; npx tauri build --config src-tauri/tauri.windows.conf.json'
```

Из-за поведения sshd длинную сборку лучше запускать detached (WMI `Win32_Process.Create`) и читать `windows-build.log`; иначе дочерние процессы могут умереть вместе с SSH-сессией.

Установщик: `src-tauri\target\release\bundle\nsis\Project-S_1.0.2_x64-setup.exe`. Установка per-user: `Start-Process <installer> -ArgumentList '/S' -Wait`.

Артефакты:

```bash
scp -r "win11:C:/Users/mb/Projects/Project-S-local-CRM/src-tauri/target/release/bundle" ./artifacts/windows/
```

Правила:

- сборка — только по явной команде пользователя («а теперь делаем для Windows»);
- перед сборкой прочитать `CROSPLATFORM.md` и `AGENTS.md`;
- актуальный `master` — `d448c15`; задание под `768ec53` устарело;
- никаких веток `port/windows`, Windows-агента и коммитов с Windows;
- после сборки пользователь проверяет GUI на Windows; macOS-агент забирает артефакты и пишет статус в `CROSPLATFORM.md`.

## 5. Безопасность

- Ключ `~/.ssh/id_ed25519_win11` **не коммитить**; в репозитории только путь и fingerprint.
- Репозиторий приватный; не переводить в публичный без удаления этого файла.
- Порт 2222 не выставлять в интернет.
- В `CROSPLATFORM.md` и этот файл не добавлять пароли, приватные ключи и реальные клиентские данные.

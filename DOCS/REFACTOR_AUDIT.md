# REFACTOR_AUDIT — `unwrap`/`expect` в production-путях (срез R-04)

Дата: 2026-10-22. Ветка `master`, коммитов по срезу нет.

## 1. Как считали

Инвентарь построен не «грубым grep», а по фактическим местам, которые видит
компилятор:

```bash
cd src-tauri
cargo clippy --all-targets -- -A clippy::all -W clippy::unwrap_used -W clippy::expect_used
```

Дальше хиты разведены на тестовые и production по span'ам: исключены модули
`#[cfg(test)] mod tests`, отдельные `#[cfg(test)]`-функции/структуры, файлы,
подключаемые только под тестами (`mcp/mock_server.rs`, `agent/invariants.rs`
с `#![cfg(test)]`), и упоминания в комментариях (`sync_util.rs:5`).

| Категория | Мест |
|---|---|
| Всего хитов `unwrap`/`expect`/`unwrap_err` по всем таргетам | 5587 |
| Из них в тестах и тестовых модулях/файлах | 5553 |
| **Production-пути** | **34** |

План (R-04) оценивал «~33» — расхождение в 1 объясняется тем, что grep считает
строки, а clippy — вызовы: в `browser_tool.rs` на строках 305, 309, 313 по два
вызова. `unwrap_or`, `unwrap_or_default`, `unwrap_or_else` в счёт не идут: они
не паникуют. `unwrap_err` встречается только в тестах.

Файлы с production-местами (10): `agent/browser_tool.rs` 18,
`agent/harness_tools/catalog.rs` 2, `agent/harness_tools/mcp_tools.rs` 3,
`agent/notes_tools.rs` 1, `agent/project_tools.rs` 1,
`agent/settings_tools.rs` 2, `journal.rs` 3, `lib.rs` 1, `mcp/mod.rs` 1,
`scheduler/mod.rs` 2.

## 2. Список мест и классификация

Классы:
- **A — безопасно по конструкции**: значение гарантировано самим кодом
  (иррефutable match, литерал `json!`, `and_hms_opt` для валидного времени,
  сериализация карты строк). Паника невозможна без изменения самого кода.
- **B — внутренний инвариант**: паника = ошибка программиста, рядом есть
  проверяющий код (валидация аргументов, «сразу выше» открыт/установлен).
  Риск — дрейф: проверяющий код и место `unwrap` разъезжаются при правке.
- **C — реальный риск**: вход извне или отравление мьютекса; паника достижима
  в рантайме и роняет команду/инструмент/процесс.

### agent/browser_tool.rs (18)

| Строка | Функция | Код | Класс |
|---|---|---|---|
| 75 | `with_session` | `guard.as_mut().expect("session set before action")` | B — сессия создаётся/поднимается несколькими строками выше; при `launch()` ошибке выход раньше |
| 258 | `execute` | `resolve_upload_path(path_arg.unwrap(), …)` | B |
| 285 | `execute` | `browser.navigate(url.unwrap())` | B |
| 288 | `execute` | `browser.click(selector.unwrap())` | B |
| 289 | `execute` | `browser.eval(expression.unwrap())` | B |
| 305 (×2) | `execute` | `.type_text(selector.unwrap(), text.unwrap())` | B |
| 307 | `execute` | `browser.press_key(key.unwrap())` | B |
| 309 (×2) | `execute` | `.select_value(selector.unwrap(), value.unwrap())` | B |
| 311 | `execute` | `browser.eval_json(expression.unwrap())` | B |
| 313 (×2) | `execute` | `.set_file_input(selector.unwrap(), upload_file.as_ref().expect("resolved above"))` | B |
| 316 | `execute` | `download_dir.as_ref().expect("resolved above")` | B |
| 349 | `execute` | `.open_tab(url.unwrap())` | B |
| 353 | `execute` | `browser.close_tab(tab_id.unwrap())` | B |
| 360 | `execute` | `browser.switch_tab(tab_id.unwrap())` | B |
| 416 | `execute` | `upload_file.expect("resolved above").display().to_string()` | B |

Все 17 (кроме 75) опираются на блок валидации действий в начале `execute`
(`"click" if selector.unwrap_or_default().is_empty() => return invalid(...)` и
т.д.): отсутствующий/пустой аргумент выходит `invalid`, дальше `Option` точно
`Some`. Инвариант держится только совпадением двух списков — действий в
валидации и действий в dispatch. Риск: новое действие добавят в dispatch и
забудут в валидацию → паника инструмента вместо `invalid_args`.

### agent/harness_tools/catalog.rs (2), agent/notes_tools.rs (1), agent/project_tools.rs (1)

| Строка | Функция | Код | Класс |
|---|---|---|---|
| catalog.rs:196 | `harness_catalog` | `catalog.register(tool, true).expect("unique tool names")` | B |
| catalog.rs:199 | `harness_catalog` | `catalog.register(tool, false).expect("unique tool names")` | B |
| notes_tools.rs:54 | `notes_catalog` | `catalog.register(tool, true).expect("unique tool names")` | B |
| project_tools.rs:128 | `project_catalog` | `catalog.register(tool, true).expect("unique tool names")` | B |

`AgentRegistry::register` возвращает `Err` на дубликат или зарезервированное
имя (`agent/catalog.rs:56`). Каталоги собираются один раз на старте, так что
дубликат имени инструмента = ошибка разработчика и падение приложения при
запуске. Fail-fast допустим на boot-time; в рантайме этих вызовов нет.

### agent/harness_tools/mcp_tools.rs (3)

| Строка | Функция | Код | Класс |
|---|---|---|---|
| 69 | `build_transport` | `for item in list.as_array().unwrap()` | A — внутри arm'а `Some(list @ Value::Array(_))` |
| 83 | `build_transport` | `entry["env"] = serde_json::to_value(env).unwrap()` | A — `BTreeMap<String,String>` сериализуется без ошибок |
| 96 | `build_transport` | `entry["headers"] = serde_json::to_value(headers).unwrap()` | A |

### agent/settings_tools.rs (2)

| Строка | Функция | Код | Класс |
|---|---|---|---|
| 602 | `execute` | `proposals().lock().unwrap().insert(…)` | **C** |
| 653 | `execute` | `proposals().lock().unwrap().remove(token)` | **C** |

`proposals()` — `static PROPOSALS: OnceLock<Mutex<HashMap<…>>>`
(`settings_tools.rs:486`): глобальный лок. Если паника случится, пока лок
удерживается, `PoisonError` отравляет его навсегда, и каждая последующая
настройка из чата падает. В проекте для этого уже есть
`sync_util::lock_recover` (110 использований) — здесь он не применён.

### journal.rs (3)

| Строка | Функция | Код | Класс |
|---|---|---|---|
| 82 | `log_event` | `record.as_object_mut().expect("json! object")` | A — литерал `json!({...})` выше |
| 108 | `log_event` | `guard.as_mut().expect("writer just opened")` | B — `guard.replace(...)` за несколько строк до |
| 247 | `mask_key_value_pairs` | `text[i..].chars().next().expect("char boundary")` | A — `i` — граница символов и `i < len` по условию цикла |

### lib.rs (1)

| Строка | Функция | Код | Класс |
|---|---|---|---|
| 185 | `run` | `tauri::Builder::default()….expect("error while building tauri application")` | B — сборка приложения: без приложения продолжать нечем, падение на старте корректно |

### mcp/mod.rs (1)

| Строка | Функция | Код | Класс |
|---|---|---|---|
| 363 | `McpToolBridge::execute` | `self.client.lock().unwrap().call_tool(…)` | **C** |

Лок `Arc<Mutex<McpClient>>` живёт всё время работы реестра MCP. Отравление
(паника в другом потоке внутри клиента) делает все инструменты этого сервера
вечно падающими. Нужен `sync_util::lock_recover`.

### scheduler/mod.rs (2)

| Строка | Функция | Код | Класс |
|---|---|---|---|
| 248 | `parse_run_at` | `NaiveDate::parse_from_str(…).map(|d| d.and_hms_opt(0, 0, 0).unwrap())` | A — `and_hms_opt(0,0,0)` для любого календарного дня возвращает `Some` |
| 387 | `create` | `compute_next(input.kind, …)?.unwrap()` | B — `create` вызывает `validate_kind` до этого, а `compute_next` возвращает `None` только для неизвестного kind |

387 — кандидат на тривиальную замену (`ok_or_else`), но поведение не меняется:
`None` достижим только если в `KINDS` добавят kind, не разобрав его в
`compute_next`. Связка «`KINDS` ↔ `compute_next`» нигде не проверяется — это и
есть цена такого `unwrap`.

## 3. Итог по классам

| Класс | Мест | Что с ними делать |
|---|---|---|
| A — безопасно по конструкции | 6 | Не трогать. При правке файла можно заменить на деструктуризацию, если это удешевляет чтение |
| B — внутренний инвариант | 25 | Не трогать массово. Требовать рядом текст `expect`, называющий инвариант, и тест на инвариант. Разобрать точечно в доменных срезах (R-09, R-11, R-12) |
| C — реальный риск | 3 | Отдельный маленький срез: `settings_tools.rs:602,653` и `mcp/mod.rs:363` → `sync_util::lock_recover` |

Массовых замен в этом срезе не делалось (требование R-04): изменённый
production-код = только удалённый мёртвый код из R-03.

## 4. Политика на будущий код

1. **Внешние данные — только `Result`.** JSON извне (аргументы инструмента,
   ответы модели, MCP-результаты, конфиги, БД, файлы, сеть, env) не
   разворачивать через `unwrap`/`expect`. Возврат `Err(AppError::Invalid/…)` с
   текстом, по которому понятно, что именно не так.
2. **Лки — через `sync_util`.** Для всего, что живёт дольше одной функции
   (`static`/`OnceLock`/`Lazy`/`Arc<Mutex<…>>` в `AppState`, реестрах,
   журнале, MCP-клиентах), использовать `lock_recover`/`read_recover`/
   `write_recover`. Обычный `.lock().unwrap()` допустим только для локального
   лока, созданного внутри той же функции.
3. **`expect` — только для инвариантов разработчика**, и только если
   выполняются все четыре условия: нарушение невозможно при валидном коде;
   рядом есть код, который инвариант обеспечивает; сообщение `expect`
   называет инвариант («resolved above», «unique tool names»); инвариант
   закрыт тестом. Иначе — `ok_or_else(|| AppError::…)?`.
4. **После старта — никаких паник.** В IPC-командах, инструментах агента,
   фоновых потоках (scheduler, watcher, reindex, chat run) паника роняет
   приложение или оставляет процесс нерабочим: там только `Result`.
   Исключения: `lib.rs::run` (сборка приложения) и boot-time реестры каталогов.
5. **Проверять себя при правке.** Локально, кроме `scripts/check.sh`:
   `cargo clippy --all-targets -- -A clippy::all -W clippy::unwrap_used -W clippy::expect_used`
   и смотреть только по затронутым файлам. В gate эти линты не включены:
   5553 тестовых срабатывания затопят вывод.
6. **Boy-scout, не ревизия.** Правя файл, можно снять его же `unwrap` из
   классов A/C, если это одна-две строки. Класс B править только вместе с
   доменным срезом, который всё равно перекладывает код.

## 5. Кандидаты в отдельные срезы

- **R-04a.** `agent/browser_tool.rs` (18 мест): заменить пару «валидация +
  dispatch» на разбор аргументов в типизированный enum действия → `unwrap`
  исчезают вместе с дрейфом списков. Делать вместе с R-12b.
- **R-04b.** `agent/settings_tools.rs:602,653`, `mcp/mod.rs:363` (3 места):
  переход на `sync_util::lock_recover`. Маленький срез, поведение не меняется.
- **R-04c.** `scheduler/mod.rs:387`: `ok_or_else` вместо `unwrap` + тест, что
  неизвестный kind даёт `Invalid`, а не панику. Делать вместе с R-06.

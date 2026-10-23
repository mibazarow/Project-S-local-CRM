import { type FormEvent } from "react";
import { Card, Field } from "../shared";
import { type McpForm, type McpTransportKind } from "../forms";

export function McpFormCard({
  form,
  onFormChange,
  editing,
  busy,
  error,
  extrasOpen,
  onExtrasOpenChange,
  onSubmit,
  onCancel,
}: {
  form: McpForm;
  onFormChange: (form: McpForm) => void;
  editing: string | null;
  busy: boolean;
  error: string | null;
  extrasOpen: boolean;
  onExtrasOpenChange: (open: boolean) => void;
  onSubmit: (e: FormEvent) => void;
  onCancel: () => void;
}) {
  const mcpArgsPreview = form.args.trim().split(/\s+/).filter(Boolean);

  return (
    <Card
      title={editing === null ? "Добавить сервер" : `Изменить сервер «${editing}»`}
      desc="Сервер сохраняется в mcp.json и сразу перезапускается."
    >
      <form className="set-form" onSubmit={(e) => void onSubmit(e)}>
        <div className="set-form-section">
          <div className="set-form-section-title">Основное</div>
          <Field
            label="Имя сервера"
            hint="Латиница, цифры, дефис и подчёркивание — по имени сервера называются его инструменты."
          >
            <input
              type="text"
              value={form.name}
              onChange={(e) => onFormChange({ ...form, name: e.target.value })}
              placeholder="example-mcp"
              spellCheck={false}
            />
          </Field>
          <Field label="Тип транспорта">
            <select
              value={form.transport}
              onChange={(e) =>
                onFormChange({ ...form, transport: e.target.value as McpTransportKind })
              }
            >
              <option value="stdio">stdio (локальный процесс)</option>
              <option value="http">http (по адресу)</option>
              <option value="sse">sse (по адресу, legacy)</option>
            </select>
          </Field>
          {form.transport === "stdio" ? (
            <Field label="Команда" hint="Программа, запускающая сервер (npx, node, python).">
              <input
                type="text"
                value={form.command}
                onChange={(e) => onFormChange({ ...form, command: e.target.value })}
                placeholder="npx"
                spellCheck={false}
              />
            </Field>
          ) : (
            <Field
              label="URL сервера"
              wide
              hint="Адрес http/sse-сервера; адреса облачных метаданных запрещены."
            >
              <input
                type="text"
                value={form.url}
                onChange={(e) => onFormChange({ ...form, url: e.target.value })}
                placeholder="https://mcp.example.com/mcp"
                spellCheck={false}
              />
            </Field>
          )}
        </div>
        <details
          className="set-form-details"
          open={extrasOpen}
          onToggle={(e) => onExtrasOpenChange(e.currentTarget.open)}
        >
          <summary>Дополнительно</summary>
          {form.transport === "stdio" ? (
            <>
              <Field label="Аргументы (по одному на строку или через пробел)" wide>
                <textarea
                  rows={2}
                  value={form.args}
                  onChange={(e) =>
                    onFormChange({ ...form, args: e.target.value })
                  }
                  placeholder={"-y\n@example/mcp-server\n/path"}
                  spellCheck={false}
                />
                {mcpArgsPreview.length > 0 && (
                  <div className="mcp-chips">
                    {mcpArgsPreview.map((a, i) => (
                      <span key={i} className="mcp-chip">
                        {a}
                      </span>
                    ))}
                  </div>
                )}
              </Field>
              <Field label="Переменные окружения (необязательно)" wide hint="По одной строке KEY=VALUE.">
                <textarea
                  rows={2}
                  value={form.env}
                  onChange={(e) => onFormChange({ ...form, env: e.target.value })}
                  placeholder="API_KEY=secret — по одной строке KEY=VALUE"
                  spellCheck={false}
                />
              </Field>
            </>
          ) : (
            <Field
              label="Заголовки (необязательно)"
              wide
              hint="Строки вида Authorization: Bearer … или KEY=VALUE."
            >
              <textarea
                rows={2}
                value={form.headers}
                onChange={(e) => onFormChange({ ...form, headers: e.target.value })}
                placeholder={"Authorization: Bearer …\nX-Api-Key=…"}
                spellCheck={false}
              />
            </Field>
          )}
          <Field
            label="Таймаут вызова, сек (необязательно)"
            hint="По умолчанию 120 с. Целое число от 1 до 3600."
          >
            <input
              type="text"
              value={form.timeout}
              onChange={(e) => onFormChange({ ...form, timeout: e.target.value })}
              placeholder="120"
              spellCheck={false}
            />
          </Field>
        </details>
        <div className="set-actions">
          <button className="primary-btn small" type="submit" disabled={busy}>
            {editing === null ? "Добавить" : "Сохранить"}
          </button>
          {editing !== null && (
            <button className="ghost-btn small" type="button" onClick={onCancel}>
              Отмена
            </button>
          )}
        </div>
        {error && <div className="set-error">{error}</div>}
      </form>
    </Card>
  );
}

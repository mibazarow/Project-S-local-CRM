import type { LlmProviderDto } from "../../../api/types";
import { Card } from "../shared";
import { endpointHost } from "./presets";

export function ProviderListCard({
  providers,
  busy,
  activeMissingKey,
  onActivate,
  onEdit,
  onDelete,
  onAdd,
}: {
  providers: LlmProviderDto[] | null;
  busy: boolean;
  activeMissingKey: boolean;
  onActivate: (id: string) => void;
  onEdit: (provider: LlmProviderDto) => void;
  onDelete: (provider: LlmProviderDto) => void;
  onAdd: () => void;
}) {
  return (
    <Card
      title="Провайдеры"
      desc="Сохраняй любое число OpenAI-совместимых провайдеров; активен ровно один — переключение только здесь."
    >
      {providers === null ? (
        <div className="set-empty">Загрузка…</div>
      ) : providers.length === 0 ? (
        <div className="set-empty">Провайдеров нет. Добавь первый ниже.</div>
      ) : (
        <div className="set-list">
          {providers.map((p) => (
            <div key={p.id} className="provider-item">
              <button
                className="provider-radio"
                role="radio"
                aria-checked={p.active}
                title={p.active ? "Активный провайдер" : "Сделать активным"}
                disabled={busy}
                onClick={() => onActivate(p.id)}
              >
                <span className={p.active ? "dot on" : "dot"}></span>
              </button>
              <div className="info">
                <div className="t">{p.name}</div>
                <div className="c">{endpointHost(p.baseUrl)}</div>
              </div>
              {p.active && <span className="badge st-active">активен</span>}
              <span className="mcp-actions">
                <button className="ghost-btn small" onClick={() => onEdit(p)}>
                  Изменить
                </button>
                <button className="ghost-btn small" onClick={() => onDelete(p)}>
                  Удалить
                </button>
              </span>
            </div>
          ))}
        </div>
      )}
      {activeMissingKey && (
        <div className="provider-warn">
          У активного провайдера нет API-ключа — впиши ключ и сохрани провайдера.
        </div>
      )}
      <div className="set-actions">
        <button className="primary-btn small" onClick={onAdd} disabled={busy}>
          + Добавить провайдера
        </button>
      </div>
    </Card>
  );
}

import type { LlmProviderDto } from "../../../api/types";
import Badge from "../../shared/Badge";
import Button from "../../shared/Button";
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
              {p.active && <Badge tone="active">активен</Badge>}
              <span className="mcp-actions">
                <Button variant="ghost" size="small" onClick={() => onEdit(p)}>
                  Изменить
                </Button>
                <Button variant="ghost" size="small" onClick={() => onDelete(p)}>
                  Удалить
                </Button>
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
        <Button variant="primary" size="small" onClick={onAdd} disabled={busy}>
          + Добавить провайдера
        </Button>
      </div>
    </Card>
  );
}

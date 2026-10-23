import type { TrashDto } from "../../../api/types";
import Badge from "../../shared/Badge";
import Button from "../../shared/Button";
import { Card } from "../shared";
import { fmtLocal } from "../forms";

export function TrashCard({
  trash,
  trashError,
  trashBusyId,
  onRestore,
  onPurge,
}: {
  trash: TrashDto[] | null;
  trashError: string | null;
  trashBusyId: number | null;
  onRestore: (id: number) => void;
  onPurge: (id: number, name: string) => void;
}) {
  return (
    <Card
      title="Корзина"
      desc="Удалённые файлы, папки и задачи попадают в корзину и хранятся 30 дней."
    >
      {trash === null ? (
        <div className="set-empty">Загрузка…</div>
      ) : trash.length === 0 ? (
        <div className="set-empty">Корзина пуста.</div>
      ) : (
        <div className="set-list">
          {trash.map((t) => (
            <div key={t.id} className="skill-item">
              <div className="info">
                <div className="t">{t.name || "(без имени)"}</div>
                <div className="c">{t.originalPath}</div>
                <div className="c">удалено {fmtLocal(t.deletedAt)}</div>
              </div>
              <Badge>{t.kind}</Badge>
              <Badge tone="disabled">{t.scope}</Badge>
              <span className="item-actions">
                <Button
                  variant="ghost"
                  size="small"
                  disabled={trashBusyId !== null}
                  onClick={() => onRestore(t.id)}
                >
                  {trashBusyId === t.id ? "…" : "Восстановить"}
                </Button>
                <Button
                  variant="ghost"
                  size="small"
                  className="danger"
                  disabled={trashBusyId !== null}
                  onClick={() => onPurge(t.id, t.name)}
                >
                  Удалить навсегда
                </Button>
              </span>
            </div>
          ))}
        </div>
      )}
      {trashError && <div className="set-error">{trashError}</div>}
    </Card>
  );
}

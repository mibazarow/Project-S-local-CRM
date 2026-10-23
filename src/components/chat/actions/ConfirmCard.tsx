import type { ConfirmationRequestPayload } from "../../../api/types";
import Button from "../../shared/Button";
import { summarizeArgs } from "../chatHistory";

// Pending-confirmation card shared by all three chats (SPEC 6: one loop,
// one confirmation flow; only the routing key differs per view).
export function ConfirmCard({
  confirmation,
  onResolve,
}: {
  confirmation: ConfirmationRequestPayload;
  onResolve: (decision: "allow" | "deny" | "allowAlways") => void;
}) {
  return (
    <div className="confirm-card">
      <div className="head">
        <span className="lbl">Подтверждение</span>
        <span className="tool">{confirmation.tool}</span>
      </div>
      <div className="args">{summarizeArgs(confirmation.args)}</div>
      {confirmation.dangerReason && <div className="danger">{confirmation.dangerReason}</div>}
      <div className="actions">
        <Button variant="primary" size="small" type="button" onClick={() => onResolve("allow")}>
          Разрешить
        </Button>
        <Button variant="ghost" size="small" type="button" onClick={() => onResolve("allowAlways")}>
          Всегда разрешать
        </Button>
        <Button
          variant="ghost"
          size="small"
          className="danger"
          type="button"
          onClick={() => onResolve("deny")}
        >
          Запретить
        </Button>
      </div>
    </div>
  );
}

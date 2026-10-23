import type { ConfirmationRequestPayload } from "../../../api/types";
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
        <button className="primary-btn small" type="button" onClick={() => onResolve("allow")}>
          Разрешить
        </button>
        <button className="ghost-btn small" type="button" onClick={() => onResolve("allowAlways")}>
          Всегда разрешать
        </button>
        <button
          className="ghost-btn small danger"
          type="button"
          onClick={() => onResolve("deny")}
        >
          Запретить
        </button>
      </div>
    </div>
  );
}

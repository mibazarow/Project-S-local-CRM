import { useEffect, useState } from "react";
import * as ipc from "../../api/ipc";
import type { LicenseStatus } from "../../api/types";
import { toMessage } from "../../shared/errors";
import { useApp } from "../../state/AppStore";
import Onboarding from "./Onboarding";

export function shouldShowLicense(
  configured: boolean,
  status: LicenseStatus | null,
): boolean {
  return !configured && status !== null && status.required;
}

export default function LicenseGate() {
  const { state } = useApp();
  const [status, setStatus] = useState<LicenseStatus | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void ipc
      .licenseStatus()
      .then(setStatus)
      .catch((e) => setError(toMessage(e)));
  }, []);

  if (error) {
    return (
      <div className="onboarding">
        <div className="onboarding-card">
          <h1>Не удалось проверить лицензию</h1>
          <div className="onboarding-error">{error}</div>
        </div>
      </div>
    );
  }

  if (!status) {
    return (
      <div className="onboarding">
        <div className="onboarding-card">
          <h1>Project-S</h1>
          <p>Загрузка…</p>
        </div>
      </div>
    );
  }

  if (!shouldShowLicense(state.onboarding.configured, status)) return <Onboarding />;

  return <LicenseScreen onAccepted={setStatus} />;
}

function LicenseScreen({ onAccepted }: { onAccepted: (status: LicenseStatus) => void }) {
  const [checked, setChecked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const accept = async () => {
    if (!checked || busy) return;
    setBusy(true);
    setError(null);
    try {
      onAccepted(await ipc.licenseAccept());
    } catch (e) {
      setError(toMessage(e));
      setBusy(false);
    }
  };

  const decline = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await ipc.licenseDecline();
    } catch (e) {
      setError(toMessage(e));
      setBusy(false);
    }
  };

  return (
    <div className="onboarding">
      <div className="onboarding-card license-card">
        <h1>Лицензионное соглашение и отказ от ответственности</h1>

        <div className="license-warning">
          <div className="license-warning-title">Полная ответственность пользователя</div>
          <p>
            Project-S получает полный доступ к вашему компьютеру: файлы, терминал, SSH, браузер,
            буфер обмена, автоматизация и MCP-серверы. Все действия выполняются на ваш страх и риск.
          </p>
        </div>

        <div className="license-text">
          <p>
            <strong>1. Полный доступ к компьютеру.</strong> Приложение может читать, изменять и
            удалять файлы, выполнять команды в терминале, подключаться к серверам по SSH, управлять
            браузером, читать и изменять буфер обмена, запускать автоматизацию и подключаться к
            MCP-серверам. Доступ не ограничен рабочей папкой.
          </p>
          <p>
            <strong>2. Использование на ваш риск.</strong> Приложение предоставляется «как есть», без
            каких-либо гарантий, явных или подразумеваемых, включая пригодность для конкретных целей.
            Вы используете его целиком под свою ответственность.
          </p>
          <p>
            <strong>3. Ограничение ответственности.</strong> Разработчик не несёт ответственности за
            потерю или повреждение данных, вред оборудованию и программному обеспечению, простои,
            упущенную выгоду, нарушения безопасности и любые иные последствия использования
            приложения, включая действия AI-агента и результаты работы сторонних LLM и сервисов.
          </p>
          <p>
            <strong>4. AI может ошибаться.</strong> Ответы и действия AI не являются истиной в
            последней инстанции. Важную информацию необходимо перепроверять самостоятельно, а
            критические операции — контролировать.
          </p>
          <p>
            <strong>5. Ваша ответственность.</strong> Вы сами отвечаете за резервные копии данных и за
            соблюдение применимого законодательства и условий третьих лиц, включая лицензии на
            программы, сервисы и данные, которые обрабатывает приложение.
          </p>
          <p>
            Нажимая «Согласиться», вы подтверждаете, что прочитали эти условия и принимаете их
            полностью.
          </p>
        </div>

        <label className="license-check">
          <input
            type="checkbox"
            checked={checked}
            disabled={busy}
            onChange={(e) => setChecked(e.target.checked)}
          />
          <span>Я прочитал и принимаю условия</span>
        </label>

        {error && <div className="onboarding-error">{error}</div>}

        <div className="onboarding-actions">
          <button
            className="primary-btn"
            disabled={!checked || busy}
            onClick={() => void accept()}
          >
            Согласиться
          </button>
          <button className="ghost-btn" disabled={busy} onClick={() => void decline()}>
            Не согласиться
          </button>
        </div>
      </div>
    </div>
  );
}

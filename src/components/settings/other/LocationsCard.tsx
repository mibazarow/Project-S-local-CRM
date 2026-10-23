import { Card, Field } from "../shared";

export function LocationsCard({
  workingDir,
  configDir,
}: {
  workingDir: string | null;
  configDir: string;
}) {
  return (
    <Card title="Расположение" desc="Папки, где приложение хранит данные и настройки.">
      <Field label="Рабочая директория" wide>
        <div className="path-value">{workingDir ?? "—"}</div>
      </Field>
      <Field label="Директория настроек" wide>
        <div className="path-value">{configDir || "—"}</div>
      </Field>
    </Card>
  );
}

import { IconGrid, IconList } from "./icons";

interface Props {
  view: "list" | "grid";
  onChange: (view: "list" | "grid") => void;
}

export default function ViewToggle({ view, onChange }: Props) {
  return (
    <div className="view-toggle" title="Вид файлов">
      <button
        className={view === "list" ? "vt active" : "vt"}
        title="Список"
        onClick={() => onChange("list")}
      >
        <IconList />
      </button>
      <button
        className={view === "grid" ? "vt active" : "vt"}
        title="Сетка"
        onClick={() => onChange("grid")}
      >
        <IconGrid />
      </button>
    </div>
  );
}

import { IconGrid, IconList } from "./icons";
import Segmented from "../shared/Segmented";

interface Props {
  view: "list" | "grid";
  onChange: (view: "list" | "grid") => void;
}

export default function ViewToggle({ view, onChange }: Props) {
  return (
    <Segmented
      value={view}
      options={[
        { value: "list", icon: <IconList />, title: "Список" },
        { value: "grid", icon: <IconGrid />, title: "Сетка" },
      ]}
      onChange={onChange}
      wrapperClassName="view-toggle"
      buttonClassName="vt"
      title="Вид файлов"
    />
  );
}

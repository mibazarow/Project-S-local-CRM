import { Fragment } from "react";
import { IconHome } from "./icons";

interface Props {
  // Project-relative folder path ("" = project root).
  path: string;
  // Crumbs navigate to that folder prefix; the current segment is never a link.
  onNavigate: (dirRel: string) => void;
}

export default function Breadcrumbs({ path, onNavigate }: Props) {
  const segments = path ? path.split("/") : [];
  const atRoot = segments.length === 0;
  return (
    <div className="crumbs">
      {atRoot ? (
        <span className="crumb crumb-home" title="Корень проекта">
          <IconHome />
        </span>
      ) : (
        <button
          className="crumb crumb-home crumb-link"
          title="В корень проекта"
          onClick={() => onNavigate("")}
        >
          <IconHome />
        </button>
      )}
      {segments.map((segment, i) => {
        const dirRel = segments.slice(0, i + 1).join("/");
        const current = i === segments.length - 1;
        return (
          <Fragment key={dirRel}>
            <span className="crumb-sep">›</span>
            {current ? (
              <span className="crumb">{segment}</span>
            ) : (
              <button className="crumb crumb-link" onClick={() => onNavigate(dirRel)}>
                {segment}
              </button>
            )}
          </Fragment>
        );
      })}
    </div>
  );
}

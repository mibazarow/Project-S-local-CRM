import { Fragment } from "react";
import type { FsEntry } from "../../api/types";

interface Props {
  projectName: string;
  selected: FsEntry | null;
}

export default function Breadcrumbs({ projectName, selected }: Props) {
  const segments = selected ? selected.relPath.split("/") : [];
  return (
    <div className="crumbs">
      <span className="crumb">{projectName}</span>
      {segments.map((segment, i) => (
        <Fragment key={i}>
          <span className="crumb-sep">›</span>
          <span className="crumb">{segment}</span>
        </Fragment>
      ))}
    </div>
  );
}

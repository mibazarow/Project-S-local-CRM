import { Fragment } from "react";

interface Props {
  projectName: string;
  path: string;
  // When present, crumbs become clickable and navigate to that folder prefix
  // (grid view). When absent, crumbs are display-only (list view, unchanged).
  onNavigate?: (dirRel: string) => void;
}

export default function Breadcrumbs({ projectName, path, onNavigate }: Props) {
  const segments = path ? path.split("/") : [];
  const crumb = (label: string, dirRel: string) =>
    onNavigate ? (
      <button className="crumb crumb-link" onClick={() => onNavigate(dirRel)}>
        {label}
      </button>
    ) : (
      <span className="crumb">{label}</span>
    );
  return (
    <div className="crumbs">
      {crumb(projectName, "")}
      {segments.map((segment, i) => (
        <Fragment key={i}>
          <span className="crumb-sep">›</span>
          {crumb(segment, segments.slice(0, i + 1).join("/"))}
        </Fragment>
      ))}
    </div>
  );
}

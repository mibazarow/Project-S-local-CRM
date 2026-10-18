import { Fragment } from "react";

interface Props {
  // Project-relative folder path ("" = project root, row hidden).
  path: string;
  // When present, crumbs become clickable and navigate to that folder prefix
  // (grid view). When absent, crumbs are display-only (list view, unchanged).
  onNavigate?: (dirRel: string) => void;
}

export default function Breadcrumbs({ path, onNavigate }: Props) {
  const segments = path ? path.split("/") : [];
  if (segments.length === 0) return null;
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
      {segments.map((segment, i) => (
        <Fragment key={i}>
          {i > 0 && <span className="crumb-sep">›</span>}
          {crumb(segment, segments.slice(0, i + 1).join("/"))}
        </Fragment>
      ))}
    </div>
  );
}

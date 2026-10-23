import { useEffect, useState } from "react";
import * as ipc from "../../../api/ipc";
import type { ChatAttachment } from "../chatTypes";

export const AttachFileIcon = () => (
  <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
    <path d="M14 2v6h6" />
  </svg>
);

export const AttachXIcon = () => (
  <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
    <path d="M18 6 6 18M6 6l12 12" />
  </svg>
);

// Reads one attachment's bytes as a data URL, lazily (on mount). Agent-sent
// files (messageId + chatId known) go through the chat-scoped command, which
// resolves the savedPath by the chat's kind; user attachments keep the
// savedPath + projectId path.
function useAttachmentUrl(
  attachment: ChatAttachment,
  projectId: number | null,
  chatId: number | null,
): string | null {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    const request =
      attachment.messageId != null && chatId != null
        ? ipc.chatAttachmentDataUrl(chatId, attachment.messageId, attachment.index ?? 0)
        : ipc.attachmentDataUrl(attachment.savedPath, projectId ?? undefined);
    void request
      .then((dataUrl) => {
        if (!cancelled) setUrl(dataUrl);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [attachment.savedPath, attachment.messageId, attachment.index, projectId, chatId]);
  return url;
}

// One image attachment in a message: the bytes come from the backend as a
// data URL; until they arrive or if reading fails, a plain name chip is shown.
// Agent images are clickable (open with the default handler), user ones stay
// plain thumbnails as before.
function AttachmentImage({
  attachment,
  projectId,
  chatId,
}: {
  attachment: ChatAttachment;
  projectId: number | null;
  chatId: number | null;
}) {
  const url = useAttachmentUrl(attachment, projectId, chatId);
  const clickable = attachment.messageId != null && chatId != null;
  const open = () => {
    if (attachment.messageId == null || chatId == null) return;
    void ipc
      .chatAttachmentOpen(chatId, attachment.messageId, attachment.index ?? 0)
      .catch(() => {});
  };
  if (!url) {
    return (
      <div className="msg-attachment doc">
        <AttachFileIcon />
        <span className="cap">{attachment.name}</span>
      </div>
    );
  }
  if (!clickable) {
    return (
      <div className="msg-attachment">
        <img src={url} alt={attachment.name} />
        <div className="cap">{attachment.name}</div>
      </div>
    );
  }
  return (
    <div
      className="msg-attachment clickable"
      role="button"
      tabIndex={0}
      title="Открыть файл"
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === "Enter") open();
      }}
    >
      <img src={url} alt={attachment.name} />
      <div className="cap">{attachment.name}</div>
    </div>
  );
}

// Attachments of a message (user or agent): images as thumbnails, documents as
// a name+icon chip; a click opens the saved file with the default macOS
// handler — through the chat-scoped command for agent-sent files, through the
// savedPath + projectId one for user attachments.
export function MessageAttachments({
  attachments,
  projectId,
  chatId,
}: {
  attachments: ChatAttachment[];
  projectId: number | null;
  chatId: number | null;
}) {
  if (attachments.length === 0) return null;
  const open = (a: ChatAttachment, fallbackIndex: number) => {
    if (a.messageId != null && chatId != null) {
      void ipc.chatAttachmentOpen(chatId, a.messageId, a.index ?? fallbackIndex).catch(() => {});
    } else {
      void ipc.attachmentOpen(a.savedPath, projectId ?? undefined).catch(() => {});
    }
  };
  return (
    <>
      {attachments.map((a, i) =>
        a.isImage ? (
          <AttachmentImage key={i} attachment={a} projectId={projectId} chatId={chatId} />
        ) : (
          <div
            key={i}
            className="msg-attachment doc"
            role="button"
            tabIndex={0}
            title="Открыть файл"
            onClick={() => open(a, i)}
            onKeyDown={(e) => {
              if (e.key === "Enter") open(a, i);
            }}
          >
            <AttachFileIcon />
            <span className="cap">{a.name}</span>
          </div>
        ),
      )}
    </>
  );
}

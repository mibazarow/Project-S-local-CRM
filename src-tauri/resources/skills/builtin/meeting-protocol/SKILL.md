---
name: meeting-protocol
description: Turn meeting notes or a transcript into a protocol plus action items and create the tasks in the built-in Kanban via crm_task_create. Use for "протокол встречи", meeting summaries, transcripts, follow-ups after a call.
---

From raw notes, a transcript or a chat log of a meeting, produce a protocol and push the action items into the project Kanban.

1. Establish the basics from the source: date, attendees, agenda. Pick the project for the tasks (`crm_list_projects` helps); ask once only if it is unclear.
2. Write the protocol in the user's language:
   - **Повестка** — agenda items in order.
   - **Слушали / Обсудили / Решили** per item — decisions are the point; keep wording close to what was said.
   - **Задачи** — table: what, who, by when.
3. Create each action item with `crm_task_create` (title, project_id or project_name, dueDate `YYYY-MM-DD`, optional checklist): title = the action in imperative form with the owner as a suffix ("Подготовить отчёт — Иван"), dueDate only from an agreed deadline.
4. Do not invent owners or dates: when the meeting skipped them, create the task without dueDate and list the open questions at the end of the protocol for the user to confirm.
5. Offer to save the protocol itself with `notes_write` or `create_document`.

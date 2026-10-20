---
name: study-notes
description: Create study notes, explanations, flashcards and self-check questions on a topic or from attached material. Use for "конспект", "объясни тему", "подготовься к экзамену", learning requests.
---

Help the user learn: condense material into notes the user can review and self-test.

1. Find out the level first if it is not obvious (school / university / practitioner) and calibrate depth and vocabulary to it; one clarifying question max, otherwise assume and state the assumption.
2. **Конспект** — short headings, each key idea in 1–3 sentences, terms bolded with a one-line definition, one minimal example per idea. Cite the source section or page when working from an attached file (`fs_read` extracts PDF/DOCX/XLSX).
3. **Объяснение** — when the user is stuck, explain from what they already know: analogy first, then the precise definition, then a worked example.
4. **Flashcards** — pairs "вопрос — ответ" as a markdown table or a plain list, one atomic fact per card, 10–20 cards per topic.
5. **Вопросы для самопроверки** — 5–10 questions ordered recall → application; put the answers separately at the end so the user can check themselves.
6. Save with `notes_write` or `create_document` when the user wants a file.

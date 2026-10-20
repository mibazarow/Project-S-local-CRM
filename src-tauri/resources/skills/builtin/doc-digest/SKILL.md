---
name: doc-digest
description: Summarize a PDF/DOCX/XLSX into a digest — essence, key numbers, tables. Use when the user attaches or names a document and asks what it says, what is inside, or for a summary ("сделай выжимку", "о чем документ").
---

Turn one or more documents (PDF, DOCX, XLSX, also CSV/MD/TXT) into a digest the user can skim in a minute.

1. Read the file with `fs_read` — it extracts docx/xlsx/xlsm/pdf to text (`extracted: true`). Scanned PDFs and images: render pages with `pdf_to_image`, then run `ocr` on them.
2. Produce the digest in the user's language:
   - **Суть** — 2–4 sentences: what the document is and its main conclusion or ask.
   - **Ключевые цифры** — amounts, dates, deadlines, quantities as a list; keep units.
   - **Таблицы** — reproduce the important tables as markdown tables; for big ones keep only the decisive rows and say what was cut.
3. Cite where each fact came from: page for PDF ("с. 3"), sheet name for XLSX ("лист «Отчёт»"), section heading for DOCX. If extraction was partial or the OCR was shaky, say so.
4. If the user asks, save the digest with `create_document` (md/docx/pdf) or `notes_write`.

Never invent numbers: if a figure is missing or ambiguous, mark it as such instead of guessing.

---
name: spreadsheet-analysis
description: Analyze XLSX/CSV data — summaries, pivots, trends, charts. Use when the user asks to analyze a spreadsheet, find patterns, build a report or a graph from tabular data ("посчитай по таблице", "сводная по продажам").
---

Analyze tabular data (xlsx/xlsm/csv) and report what it says.

1. Look first: `fs_read` extracts xlsx to text; read csv directly. Check headers, row counts, value formats and missing cells before computing anything.
2. For anything beyond a quick glance use `bash` with python3 + pandas/openpyxl when available (check with `python3 -c "import pandas"`); otherwise work from the extracted text and say the analysis was done without pandas.
3. Typical deliverables: totals and group summaries, top/bottom N, trends over time, pivot tables, anomalies. State the assumptions — date range included, how blanks were treated, which column is the metric — so the user can reproduce the numbers.
4. Charts: if the user wants a picture, generate a PNG with python/matplotlib via `bash` and save it next to the source; without python, produce a markdown table and describe the trend instead.
5. Save results as a file with `create_spreadsheet` (xlsx/csv) or `create_document` for a written report when asked.

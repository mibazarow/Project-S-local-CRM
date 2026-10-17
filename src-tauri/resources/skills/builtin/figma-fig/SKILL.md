---
name: figma-fig
description: Parse Figma (.fig) files and convert the layout to HTML/CSS. Use when the user sends a .fig file or asks to "build from a Figma layout", "read .fig", "extract a design from Figma". Includes archive extraction, parsing the binary fig-kiwij format via the npm parsefig package, extracting palette/fonts/coordinates/texts/images and assembling a static prototype.
---

# Figma .fig → HTML/CSS

Parse Figma design files (.fig) and build layouts from them. Verified on a real layout with 732 nodes / 3 pages.

## When to use

- User sent a `*.fig` file (including a binary one embedded in the message)
- Request "build from the layout", "convert design to HTML", "read the layout"
- Need to extract palette, fonts, texts, coordinates, images from the layout

## Briefly: what a .fig is

`.fig` is a **ZIP archive**. Inside:
- `canvas.fig` — the main document, **proprietary binary format** (magic signature `fig-kiwij`). NOT JSON, cannot be read as plain text.
- `meta.json` — metadata (file name, background color, preview size).
- `thumbnail.png` — layout preview (can be given to the user as a reference).
- `images/` — embedded raster images (names are 40-character SHA1 hashes).

## Step 1. Verify and extract

```bash
file MED-TEX.fig              # expected: Zip archive data
mkdir -p /tmp/fig_work && unzip -o MED-TEX.fig -d /tmp/fig_work
cat /tmp/fig_work/meta.json
```

Do not try to decode `canvas.fig` as UTF-8/JSON — you will get `UnicodeDecodeError`.

## Step 2. Parse the binary document

Use the npm package `parsefig` (MIT, pulls `kiwi-schema` — a reverse-engineered Figma format).

```bash
mkdir -p /tmp/fig_parse && cd /tmp/fig_parse
npm init -y > /dev/null && npm install parsefig --no-audit --no-fund
```

API (see `node_modules/parsefig/dist/index.d.ts`):
- `parseFigFile(path)` — parses a `.fig` file entirely (unpacks canvas.fig itself)
- `parseFigBinary(buf)` — for raw `fig-kiwi/fig-kiwij` data
- result: `{ schemaDefinitions, rawMessage, nodeTree }`

**IMPORTANT**: `printTree(result)` may crash (`nodes is not iterable`) — traverse the tree manually.

## Step 3. Extract data (working script)

Key fields in `node.properties` (verified on a real file):

| What you need | Where it lives |
|-----------|-----------|
| Coordinates | `transform.m02` (x), `transform.m12` (y) |
| Size | `size.x`, `size.y` |
| Fills | **`fillPaints`** (NOT `fills`!) — array `{type:'SOLID'|'IMAGE'|'GRADIENT_*', color:{r,g,b}, opacity}` |
| Strokes | `strokePaints`, thickness `strokeWeight` |
| Radius | `cornerRadius`, `rectangleTopLeftCornerRadius`, etc. |
| Text | **`textData.characters`** (NOT `characters`!) |
| Font | `fontName` `{family, style}`, size `fontSize`, tracking `letterSpacing` |
| Node type | `node.type`: DOCUMENT/CANVAS/FRAME/TEXT/SYMBOL/INSTANCE/ROUNDED_RECTANGLE/VECTOR/… |
| Name, id | `node.name`, `node.id` |
| Visibility | `visible` |

Minimal traversal script (`/tmp/fig_parse/dump.js`):

```js
const { parseFigFile } = require('parsefig');
const r = parseFigFile(process.argv[2]);
function hex(c){return '#'+[c.r,c.g,c.b].map(v=>Math.round((v||0)*255).toString(16).padStart(2,'0')).join('');}
function walk(n,d){
  const p=n.properties||{}, t=p.transform||{};
  let s='';
  if(p.fillPaints){const f=p.fillPaints.filter(x=>x&&x.type==='SOLID')[0]; if(f)s+=' bg='+hex(f.color);}
  if(p.textData&&p.textData.characters!==undefined)s+=' «'+String(p.textData.characters).replace(/\n/g,' ').slice(0,60)+'»';
  console.log('  '.repeat(d)+n.type+' '+n.name+' @'+Math.round(t.m02||0)+','+Math.round(t.m12||0)+' '+Math.round((p.size&&p.size.x)||0)+'x'+Math.round((p.size&&p.size.y)||0)+s);
  if(Array.isArray(n.children))n.children.forEach(c=>walk(c,d+1));
}
walk(r.nodeTree[0],0);
```

Useful aggregations: build a `Map` of unique colors (frequency → palette) and unique `(family|style|fontSize)` → typography.

## Step 4. Images

Extract from the archive and save into the prototype folder (hash names):

```bash
mkdir -p _design/assets/img
unzip -o -j MED-TEX.fig "images/*" -d _design/assets/img
```

**GOTCHA**: the parser does NOT return `imageRef` (it is `null` in `fillPaints`), and the 40-hex strings in `rawMessage` do NOT match the file names in the archive. So "hash → layout slot" is matched **by context**: node names (`DC2020`, `Podgolovnik-12_06702`, `stratovulkan_…`, `qr_vk`) and area proportions (`node.size`). If an exact match is not critical — assign in order and document it in the README.

Image dimensions: `python3` reading PNG/JPEG headers, or `sips -g pixelWidth -g pixelHeight`.

## Step 5. Build the prototype

- Folder `_design/`: `index.html`, `catalog.html`, `card.html`, `css/style.css`, `assets/img/`, `README.md`.
- Put shared styles into a single `style.css` with CSS-variable design tokens (`--blue`, `--red`, …).
- Logos — **simplified SVG analogues** (original vector outlines are in a closed format and cannot be extracted): colored square/gear + text.
- Icons — inline SVG (line style), no external libraries.
- Repeat header/footer sections on every page (for Bitrix migration this matches `header.php`/`footer.php`).
- Slider/filters/cart — static; add JS if needed.

## Step 6. Verification

```bash
# 1) all image references exist
grep -ohE 'assets/img/[0-9a-f]{40}' _design/*.html | sort -u | while read ref; do \
  f="${ref#assets/img/}"; [ -f "_design/$f" ] || echo "MISS $ref"; done
# 2) tag validity — html.parser (Python): unclosed/extra tags
```

## Gotchas (verified)

1. `canvas.fig` is binary `fig-kiwij`, do NOT decode as text.
2. Fills: `fillPaints`, not `fills`. Text: `textData.characters`, not `characters`.
3. `printTree(result)` may throw `TypeError` — traverse the tree manually.
4. `imageRef` is `null` in the parser — match images by context and document it.
5. `absoluteBoundingBox` gives `NaN` on some nodes — use `transform.m02/m12`.
6. Typos in 40-character hashes when pasting into HTML by hand — cross-check against real file names (`find`/`ls`).
7. If using `write_to_file` produces a file named `_` instead of the target path — move the content and delete the extra file.
8. Project rules: do not compile SCSS, do not create minified CSS/JS (if specified in user instructions).

## Useful

- Reference: save the full layer tree to `.txt` and hand it to the user as an artifact.
- Show the `thumbnail.png` preview to the user for visual comparison.
- If an exact frame render is needed — separate converters/export via the Figma REST API exist (requires an access token), but for layout building the node tree + palette + texts is enough.

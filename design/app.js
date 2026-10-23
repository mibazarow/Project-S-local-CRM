/* Project S — прототип UI: моковые данные и интерактив */

// ---------- Моковые данные ----------

const MOCK_IMG = 'data:image/svg+xml;utf8,' + encodeURIComponent(
  '<svg xmlns="http://www.w3.org/2000/svg" width="320" height="200">' +
  '<rect width="320" height="200" fill="#f2f4f7"/>' +
  '<rect width="320" height="34" fill="#1d1d1f"/>' +
  '<text x="12" y="22" font-family="sans-serif" font-size="12" fill="#fff">example.ru</text>' +
  '<rect x="12" y="46" width="296" height="70" rx="6" fill="#0a84ff"/>' +
  '<text x="24" y="86" font-family="sans-serif" font-size="14" fill="#fff">Крупный баннер перекрывает заголовок</text>' +
  '<rect x="12" y="128" width="180" height="12" rx="3" fill="#c9ccd2"/>' +
  '<rect x="12" y="148" width="140" height="12" rx="3" fill="#d8dbe0"/>' +
  '<rect x="12" y="172" width="90" height="18" rx="4" fill="#e4e6ea"/>' +
  '</svg>'
);

const MOCK_FILE = { kind: 'file', name: 'Счет_этап_2_черновик.docx' };

const PROJECTS = {
  'example.ru': {
    files: {
      'datas.md': { type: 'file', preview: '# Доступы — example.ru\n\n## Хостинг (Beget)\nЛогин: petrov\nПароль: ********\nПанель: https://beget.com\n\n## FTP\nХост: ftp.example.ru\nПользователь: example_admin\n\n## WordPress\nАдминка: https://example.ru/wp-admin\nЛогин: admin\n\n## Домен (reg.ru)\nДоговор №001-28-09-26, продление до 28.09.2027' },
      'Договор конечный': { type: 'dir', children: {
        'EXAMPLE_Dogovor_Final.docx': { type: 'file' },
        'EXAMPLE_Obem_rabot_Final.docx': { type: 'file' },
        'Счет_предоплата_ИП_Петров_EXAMPLE.docx': { type: 'file' },
        'Приложение №2. Наглядные примеры.pdf': { type: 'file' },
      }},
      'Договоры черновики': { type: 'dir', children: {
        'CONTEXT.md': { type: 'file', preview: '# Контекст проекта example.ru\n\nКлиент: EXAMPLE\nДоговор: № 001-28-09-26 (подписан 28.09.2026)\nЭтап 1: аудит сайта — принят, акт подписан.\nЭтап 2: редизайн разделов — в работе до 15.11.\n\nБаги, найденные при аудите — в old/Баги_2026_09.pdf' },
        'ТЗ_example.ru_2026_09.docx': { type: 'file' },
        'Перечень работ (example.ru).docx': { type: 'file' },
      }},
    },
    tasks: [
      { t: 'Согласовать правки ТЗ (этап 2)', s: 'progress', m: 'до 15.10', overdue: true },
      { t: 'Выставить счёт за этап 2', s: 'new', m: 'после ТЗ' },
      { t: 'Акт по этапу 1', s: 'done', m: 'подписан 02.10' },
      { t: 'Обновить доступы к FTP в datas.md', s: 'done', m: '16.10' },
    ],
    chat: [
      { who: 'agent', text: 'Проект example.ru: договор №001-28-09-26 подписан, этап 1 принят, этап 2 (редизайн) в работе до 15.11. Открытых задач — 2. Чем помочь?' },
      { who: 'user', text: 'найди доступы к хостингу' },
      { who: 'agent', steps: ['поиск по индексу проекта', 'чтение datas.md'], text: 'Хостинг Beget: логин petrov, панель beget.com. FTP: ftp.example.ru, пользователь example_admin. Пароли — в datas.md, раздел «Хостинг».' },
      { who: 'user', text: 'вот скрин главной — сохраните в проект и скажите, что не так', attachments: [{ kind: 'image', name: 'screen-glavnaya.png', url: MOCK_IMG }] },
      { who: 'agent', steps: ['зрение: разбор изображения', 'запись: example.ru/Входящие/screen-glavnaya.png', 'запись: example.ru/INBOX.md'], text: 'Сохранил в «Входящие», записал в INBOX.md. На скрине: шапка без CTA, баннер перекрывает заголовок H1, форма заявки ниже первого экрана. Рекомендую вынести CTA в шапку — заведу задачу?' },
    ],
  },
  'kedr.example.ru': {
    files: {
      'datas.md': { type: 'file', preview: '# Доступы — kedr.example.ru\n\n## Домен\nРегистратор: reg.ru\n\n## Почта\noffice@kedr.example.ru' },
      'Документы': { type: 'dir', children: {
        'Договор_КЕДР_Групп.docx': { type: 'file' },
        'Доп_соглашение_1.docx': { type: 'file' },
        'Счет_брендбук.docx': { type: 'file' },
      }},
      'Брендбук': { type: 'dir', children: {
        'Кедр Групп.pdf': { type: 'file' },
        'логотип': { type: 'dir', children: { 'logo.svg': { type: 'file' } } },
        'шрифты': { type: 'dir', children: { 'Kedr-Book.otf': { type: 'file' } } },
      }},
    },
    tasks: [
      { t: 'Передать исходники логотипа клиенту', s: 'new', m: 'до 25.10' },
      { t: 'Акт по брендбуку', s: 'progress', m: 'шаблон готов' },
    ],
    chat: [
      { who: 'agent', text: 'Проект kedr.example.ru (клиент «КЕДР Групп»): брендбук передан, остался акт. Спроси что-нибудь.' },
    ],
  },
  'apteka.example.ru': {
    files: {
      'datas.md': { type: 'file', preview: '# Доступы — apteka.example.ru\n\n## Сервер (VPS)\nIP: 192.0.2.15, SSH-ключ в ~/.ssh\n\n## WordPress + мобильное приложение\nAPI-ключи — раздел DATAS_PROJECT_1' },
      'Документы': { type: 'dir', children: {
        'Договор_ЗДРАВСИСТЕМА_ИП Петров.pdf': { type: 'file' },
        'Карточка предприятия.docx': { type: 'file' },
        'Сделано': { type: 'dir', children: {
          'Счет_1.docx': { type: 'file' },
          'АКТЫ': { type: 'dir', children: { 'АКТ_этап_3.docx': { type: 'file' } } },
        }},
      }},
    },
    tasks: [
      { t: 'Счёт за мобильное приложение (предоплата)', s: 'progress', m: 'счёт готов', c: 'ждём ответ заказчика' },
      { t: 'Дописать АКТЫ за 4 этап', s: 'new', m: 'до 12.10', overdue: true },
      { t: 'Оплатить ИИ-сервисы', s: 'done', m: '12.10' },
    ],
    chat: [
      { who: 'agent', text: 'apteka.example.ru: 7 счетов выставлено, акты за этапы 1–3 подписаны, по моб. приложению ждём предоплату.' },
    ],
  },
  'temp-test.ru': {
    files: {
      'datas.md': { type: 'file', preview: '# Доступы — temp-test.ru\n\nПока пусто.' },
      'Договор черновик': { type: 'dir', children: {
        'brif_petrov_ru.docx': { type: 'file' },
        'Договор_ИП Петров_ред.docx': { type: 'file' },
        'Приложение_№1_ТЗ.docx': { type: 'file' },
      }},
    },
    tasks: [
      { t: 'Отправить договор клиенту', s: 'new', m: '' },
    ],
    chat: [
      { who: 'agent', text: 'temp-test.ru: договор в черновике, клиент не подтверждён. Помогу причесать ТЗ перед отправкой.' },
    ],
  },
};

const SESSIONS = [
  { id: 's1', name: 'Курсы валют и закупки', meta: 'каждый день в 10:00 · активна 3 дня' },
  { id: 's2', name: 'Разбор папок OLD_FOLDER', meta: 'последняя активность: 5 дней назад' },
  { id: 's3', name: 'Мониторинг доменов клиентов', meta: 'раз в неделю · активна' },
];

const HARNESS_CHATS = {
  s1: [
    { who: 'user', text: 'ставь себе расписание: каждый день в 10 утра присылай курсы доллара и евро и пиши, если закупка сервера подорожала' },
    { who: 'agent', steps: ['создано расписание «Курсы валют» (cron: ежедневно 10:00)', 'браузер: cbr.ru — курсы на сегодня'], text: 'Готово. Каждый день в 10:00 буду присылать курсы. Сегодня: USD 82,41 (+0,3%), EUR 95,12 (-0,1%). Закупка сервера Beget на год: 14 400 ₽ — без изменений.' },
    { who: 'user', text: 'вот черновик счёта за этап 2 — проверь и сохрани в проект example.ru', attachments: [MOCK_FILE] },
    { who: 'agent', steps: ['чтение docx', 'заметки: сумма не сходится с ТЗ', 'запись: example.ru/Договоры черновики/Счет_этап_2_черновик.docx'], text: 'Сохранил в проект. Проверил: сумма 120 000 ₽, по ТЗ этапа — 150 000 ₽. Исправить и перегенерировать?' },
  ],
  s2: [
    { who: 'user', text: 'разбери OLD_FOLDER: где у меня лежат все счета по ЗДРАВСИСТЕМА?' },
    { who: 'agent', steps: ['bash: ls OLD_FOLDER/apteka.example.ru/Документы/Сделано', 'чтение 7 файлов .docx'], text: 'Все 7 счетов по ЗДРАВСИСТЕМА — в apteka.example.ru/Документы/Сделано/. Ещё 3 без актов — в «Сделано нет актов». Могу перенести в Project S и завести задачи по актам.' },
  ],
  s3: [
    { who: 'agent', steps: ['браузер: reg.ru — срок домена example.ru'], text: 'Домены клиентов в порядке. example.ru — до 28.09.2027, apteka.example.ru — до 14.03.2027. Напомню за 30 дней.' },
  ],
};

const SCHEDULES = [
  { t: 'Курсы валют (USD/EUR/CNY)', c: 'ежедневно, 10:00', on: true },
  { t: 'Проверка сроков доменов', c: 'каждый понедельник, 09:00', on: true },
  { t: 'Сводка открытых задач по проектам', c: 'пятница, 18:00', on: false },
];

const MEMORY = [
  'Клиент ЗДРАВСИСТЕМА (ООО, аптечная сеть) — проект apteka.example.ru',
  'Пользователь — ИП Петров, исполнитель по всем договорам',
  'Счета всегда в .docx, нумерация сквозная по году',
];

const SKILLS = [
  { name: 'ponytail', kind: 'встроенный', on: true },
  { name: 'research', kind: 'встроенный', on: true },
  { name: 'tdd', kind: 'встроенный', on: false },
  { name: 'мой-протокол-встреч', kind: 'пользовательский', on: true },
];

const MCP_SERVERS = [
  { name: 'fetch', kind: 'stdio', status: 'запущен', on: true },
  { name: 'gdrive', kind: 'HTTP', status: 'ошибка запуска', on: false },
];

// ---------- Утилиты ----------

const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];

const ICONS = {
  folder: '<svg viewBox="0 0 24 24" width="15" height="15"><path d="M2 5.9A1.9 1.9 0 0 1 3.9 4h4.4c.6 0 1.12.24 1.5.66L10.9 5.8H20.1A1.9 1.9 0 0 1 22 7.7v1.5H2V5.9z" fill="#2f7fe0"/><path d="M2 9.2h20v8.9a1.9 1.9 0 0 1-1.9 1.9H3.9A1.9 1.9 0 0 1 2 18.1V9.2z" fill="#5ba7f5"/></svg>',
  file: '<svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5z"/><path d="M14 3v5h5"/></svg>',
  md: '<svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="2"/><path d="M7 15V9l2.5 3L12 9v6M16 9v4m0 0 1.8-2M16 13l-1.8-2"/></svg>',
  key: '<svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="15" r="4"/><path d="m10.8 12.2 8.2-8.2M15 5l3 3M18 2l3 3"/></svg>',
  pencil: '<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M17 3a2.8 2.8 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z"/></svg>',
  chev: '<svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 6 6 6-6 6"/></svg>',
  cmt: '<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>',
  copy: '<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>',
  trash: '<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6M10 11v6M14 11v6"/></svg>',
};

const TEXT_EXT = ['md', 'txt'];

function toast(text) {
  const t = $('#toast');
  t.textContent = text;
  t.hidden = false;
  clearTimeout(toast._t);
  toast._t = setTimeout(() => (t.hidden = true), 2200);
}

// ---------- Вкладки ----------

$$('.tab').forEach((tab) =>
  tab.addEventListener('click', () => {
    $$('.tab').forEach((t) => t.classList.toggle('active', t === tab));
    $$('.view').forEach((v) => v.classList.remove('active'));
    $(`.view-${tab.dataset.tab}`).classList.add('active');
  })
);

// ---------- Проекты ----------

let currentProject = 'example.ru';

function renderProjectList() {
  const el = $('#projectList');
  el.innerHTML = '';
  Object.keys(PROJECTS).forEach((name) => {
    const p = PROJECTS[name];
    const nFiles = countFiles(p.files);
    const nTasks = p.tasks.filter((t) => t.s !== 'done').length;
    const nOver = p.tasks.filter((t) => t.overdue && t.s !== 'done').length;
    const btn = document.createElement('button');
    btn.className = 'project-item' + (name === currentProject ? ' active' : '');
    btn.innerHTML =
      `<span class="name">${name}</span><span class="meta">${nFiles} файлов · ${nTasks} задач` +
      (nOver ? ` · <span class="badge-red">${nOver} просрочено</span>` : '') + `</span>` +
      `<span class="proj-actions"><button class="mini-btn" data-act="rename" title="Переименовать проект">${ICONS.pencil}</button></span>`;
    $('[data-act="rename"]', btn).addEventListener('click', (e) => {
      e.stopPropagation();
      startProjectRename(btn, name);
    });
    btn.addEventListener('click', () => { currentProject = name; folderPath = []; renderAll(); });
    el.appendChild(btn);
  });
}

function startProjectRename(item, name) {
  const label = $('.name', item);
  const input = document.createElement('input');
  input.className = 'rename-input';
  input.value = name;
  label.replaceWith(input);
  input.focus();
  input.select();
  const commit = () => {
    const next = input.value.trim();
    if (next && next !== name && !(next in PROJECTS)) {
      const rebuilt = {};
      for (const key of Object.keys(PROJECTS)) rebuilt[key === name ? next : key] = PROJECTS[key];
      Object.keys(PROJECTS).forEach((k) => delete PROJECTS[k]);
      Object.assign(PROJECTS, rebuilt);
      if (currentProject === name) currentProject = next;
    }
    renderAll();
  };
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') commit();
    if (e.key === 'Escape') renderProjectList();
  });
  input.addEventListener('blur', commit);
  input.addEventListener('click', (e) => e.stopPropagation());
  input.addEventListener('mousedown', (e) => e.stopPropagation());
}

function countFiles(node) {
  return Object.values(node).reduce((acc, n) => acc + (n.type === 'file' ? 1 : countFiles(n.children)), 0);
}

let fileView = 'list';
let folderPath = [];

function renderTree() {
  const el = $('#fileTree');
  el.innerHTML = '';
  if (fileView === 'grid') {
    el.appendChild(buildCrumbs());
    el.appendChild(buildGrid());
  } else {
    el.appendChild(buildTree(PROJECTS[currentProject].files, true));
  }
  $('#projectTitle').textContent = currentProject;
  $('#chatProject').textContent = currentProject;
  $('#filePreview').hidden = true;
  currentPreview = null;
}

$('#fileViewToggle').addEventListener('click', (e) => {
  const b = e.target.closest('.vt');
  if (!b) return;
  fileView = b.dataset.view;
  $$('.vt', $('#fileViewToggle')).forEach((x) => x.classList.toggle('active', x === b));
  renderTree();
});

function sortEntries(obj) {
  return Object.entries(obj).sort((a, b) => (a[1].type === b[1].type ? a[0].localeCompare(b[0]) : a[1].type === 'dir' ? -1 : 1));
}

function iconFor(name, node) {
  return node.type === 'dir' ? ICONS.folder : name === 'datas.md' ? ICONS.key : name.endsWith('.md') ? ICONS.md : ICONS.file;
}

function actionButtons(node) {
  return (
    `<button class="mini-btn" data-act="rename" title="Переименовать">${ICONS.pencil}</button>` +
    (node.type === 'file' ? `<button class="mini-btn" data-act="copy" title="Дублировать">${ICONS.copy}</button>` : '') +
    `<button class="mini-btn danger" data-act="del" title="Удалить">${ICONS.trash}</button>`
  );
}

function buildCrumbs() {
  const div = document.createElement('div');
  div.className = 'crumbs';
  const mk = (label, depth) => {
    const b = document.createElement('button');
    b.className = 'crumb';
    b.textContent = label;
    b.addEventListener('click', () => { folderPath.length = depth; renderTree(); });
    return b;
  };
  div.appendChild(mk(currentProject, 0));
  folderPath.forEach((p, i) => {
    const sep = document.createElement('span');
    sep.className = 'crumb-sep';
    sep.textContent = '›';
    div.appendChild(sep);
    div.appendChild(mk(p, i + 1));
  });
  return div;
}

function buildGrid() {
  let container = PROJECTS[currentProject].files;
  for (const p of folderPath) container = container[p].children;
  const wrap = document.createElement('div');
  wrap.className = 'file-grid';
  for (const [name, node] of sortEntries(container)) {
    const tile = document.createElement('div');
    tile.className = 'tile';
    const ext = name.includes('.') ? name.split('.').pop() : '';
    tile.innerHTML =
      `<span class="tile-actions">${actionButtons(node)}</span>` +
      `<span class="tile-icon${node.type === 'dir' ? ' folder-icon' : ''}">${iconFor(name, node)}</span>` +
      `<span class="label">${name}</span>` +
      (ext ? `<span class="tag">${ext}</span>` : '');
    $$('[data-act]', tile).forEach((btn) =>
      btn.addEventListener('click', (e) => {
        e.stopPropagation();
        const act = btn.dataset.act;
        if (act === 'rename') startRename(tile, name, container);
        if (act === 'copy') duplicateEntry(name, container);
        if (act === 'del') {
          delete container[name];
          renderTree();
          renderProjectList();
          toast(`Удалено: ${name}`);
        }
      })
    );
    if (node.type === 'dir') {
      tile.addEventListener('click', () => { folderPath.push(name); renderTree(); });
    } else {
      tile.addEventListener('click', () => {
        $$('.tile.selected').forEach((r) => r.classList.remove('selected'));
        tile.classList.add('selected');
        showPreview(name, node);
      });
      tile.addEventListener('dblclick', () => {
        if (!TEXT_EXT.includes(name.split('.').pop())) toast(`«${name}» откроется во внешнем редакторе (двойной клик)`);
      });
    }
    wrap.appendChild(tile);
  }
  return wrap;
}

function buildTree(container, root = false) {
  const ul = document.createElement('ul');
  ul.className = root ? 'tree-children root' : 'tree-children nested';
  if (root) ul.style.paddingLeft = '0';
  const entries = sortEntries(container);
  for (const [name, node] of entries) {
    const li = document.createElement('li');
    li.className = 'tree-node';
    const row = document.createElement('div');
    row.className = 'tree-row';
    const icon = iconFor(name, node);
    const ext = name.includes('.') ? name.split('.').pop() : '';
    row.innerHTML =
      (node.type === 'dir' ? `<span class="chev">${ICONS.chev}</span>` : '') +
      `<span class="icon${node.type === 'dir' ? ' folder-icon' : ''}">${icon}</span><span class="label${node.type === 'dir' ? ' folder-label' : ''}">${name}</span>` +
      `<span class="row-actions">${actionButtons(node)}</span>` +
      (ext ? `<span class="tag">${ext}</span>` : '');
    li.appendChild(row);

    $$('[data-act]', row).forEach((btn) =>
      btn.addEventListener('click', (e) => {
        e.stopPropagation();
        const act = btn.dataset.act;
        if (act === 'rename') startRename(row, name, container);
        if (act === 'copy') duplicateEntry(name, container);
        if (act === 'del') {
          delete container[name];
          renderTree();
          renderProjectList();
          toast(`Удалено: ${name}`);
        }
      })
    );

    if (node.type === 'dir') {
      let open = false;
      let child = null;
      row.addEventListener('click', () => {
        open = !open;
        row.classList.toggle('open', open);
        if (open && !child) { child = buildTree(node.children); li.appendChild(child); }
        if (child) child.style.display = open ? '' : 'none';
      });
    } else {
      row.addEventListener('click', () => {
        $$('.tree-row.selected').forEach((r) => r.classList.remove('selected'));
        row.classList.add('selected');
        showPreview(name, node);
      });
      row.addEventListener('dblclick', () => {
        if (!TEXT_EXT.includes(name.split('.').pop())) toast(`«${name}» откроется во внешнем редакторе (двойной клик)`);
      });
    }
    ul.appendChild(li);
  }
  return ul;
}

function startRename(row, name, container) {
  const label = $('.label', row);
  const input = document.createElement('input');
  input.className = 'rename-input';
  input.value = name;
  label.replaceWith(input);
  input.focus();
  const dot = name.lastIndexOf('.');
  if (dot > 0) input.setSelectionRange(0, dot); else input.select();
  const commit = () => {
    const next = input.value.trim();
    if (next && next !== name && !(next in container)) {
      container[next] = container[name];
      delete container[name];
      renderTree();
    } else {
      renderTree();
    }
  };
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') commit();
    if (e.key === 'Escape') renderTree();
  });
  input.addEventListener('blur', commit);
  input.addEventListener('click', (e) => e.stopPropagation());
}

function duplicateEntry(name, container) {
  const dot = name.lastIndexOf('.');
  const base = dot > 0 ? name.slice(0, dot) : name;
  const ext = dot > 0 ? name.slice(dot) : '';
  let candidate = base + ' copy' + ext;
  let i = 2;
  while (candidate in container) candidate = base + ` copy ${i++}` + ext;
  container[candidate] = JSON.parse(JSON.stringify(container[name]));
  renderTree();
  renderProjectList();
  toast(`Дубликат: ${candidate}`);
}

let currentPreview = null;

// shortcut: рендерит заголовки/списки/жирный/курсив/инлайн-код/hr; таблицы и вложенные списки — при переходе на реальный md-парсер
function mdRender(src) {
  const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;');
  const inline = (s) =>
    esc(s)
      .replace(/\*\*([^*].*?[^*])\*\*/g, '<strong>$1</strong>')
      .replace(/\*([^*]+?)\*/g, '<em>$1</em>')
      .replace(/`(.+?)`/g, '<code>$1</code>');
  const out = [];
  let inList = false;
  for (const line of src.split('\n')) {
    const h = line.match(/^(#{1,6})\s+(.*)/);
    const li = line.match(/^\s*[-*]\s+(.*)/) || line.match(/^\s*\d+\.\s+(.*)/);
    if (inList && !li) { out.push('</ul>'); inList = false; }
    if (h) { out.push(`<h${Math.min(h[1].length + 2, 6)}>${inline(h[2])}</h${Math.min(h[1].length + 2, 6)}>`); continue; }
    if (/^(-{3,}|\*{3,})\s*$/.test(line)) { out.push('<hr>'); continue; }
    if (li) { if (!inList) { out.push('<ul>'); inList = true; } out.push(`<li>${inline(li[1])}</li>`); continue; }
    if (line.trim() === '') continue;
    out.push(`<p>${inline(line)}</p>`);
  }
  if (inList) out.push('</ul>');
  return out.join('');
}

function showPreview(name, node) {
  currentPreview = { name, node };
  $('#previewName').textContent = name;
  const editable = !!node.preview;
  $('#previewEdit').hidden = !editable;
  $('#previewEdit').textContent = 'Редактировать';
  const body = $('#previewBody');
  body.hidden = false;
  if (!editable) {
    body.className = 'preview-body';
    body.textContent = '— docx/pdf открываются внешним редактором по двойному клику; для md/txt — просмотр и редактирование здесь —';
  } else {
    body.className = 'preview-body md';
    body.innerHTML = mdRender(node.preview);
  }
  const ta = $('#previewTextarea');
  ta.hidden = true;
  $('#filePreview').hidden = false;
}

$('#previewEdit').addEventListener('click', () => {
  if (!currentPreview) return;
  const { name, node } = currentPreview;
  const body = $('#previewBody');
  const ta = $('#previewTextarea');
  const btn = $('#previewEdit');
  if (btn.textContent === 'Редактировать') {
    ta.value = node.preview;
    body.hidden = true;
    ta.hidden = false;
    btn.textContent = 'Готово';
    ta.focus();
  } else {
    node.preview = ta.value;
    ta.hidden = true;
    body.hidden = false;
    body.className = 'preview-body md';
    body.innerHTML = mdRender(node.preview);
    btn.textContent = 'Редактировать';
    toast(`Сохранено: ${name}`);
  }
});

$('#previewClose').addEventListener('click', () => {
  $('#filePreview').hidden = true;
  currentPreview = null;
  $$('.tree-row.selected').forEach((r) => r.classList.remove('selected'));
});

// ---------- Центр: файлы/задачи ----------

$$('#centerTabs .seg').forEach((seg) =>
  seg.addEventListener('click', () => {
    $$('#centerTabs .seg').forEach((s) => s.classList.toggle('active', s === seg));
    $('#centerFiles').hidden = seg.dataset.center !== 'files';
    $('#centerTasks').hidden = seg.dataset.center !== 'tasks';
  })
);

function renderTasks() {
  const tasks = PROJECTS[currentProject].tasks;
  ['new', 'progress', 'done'].forEach((status) => {
    const col = $(`.task-list[data-status="${status}"]`);
    col.innerHTML = '';
    const items = tasks.filter((t) => t.s === status);
    $(`#cnt${status === 'new' ? 'New' : status === 'progress' ? 'Progress' : 'Done'}`).textContent = items.length;
    items.forEach((t) => {
      const card = document.createElement('div');
      card.className = 'task-card' + (status === 'done' ? ' done' : '') + (t.overdue && status !== 'done' ? ' overdue' : '');
      card.draggable = true;
      card.innerHTML =
        `<div class="t">${t.t}</div><div class="m${t.overdue && status !== 'done' ? ' red' : ''}">${t.m || ''}</div>` +
        (t.c ? `<div class="c">${t.c}</div>` : '') +
        `<span class="task-actions">` +
        `<button class="mini-btn" data-act="edit" title="Редактировать">${ICONS.pencil}</button>` +
        `<button class="mini-btn" data-act="cmt" title="Комментарий">${ICONS.cmt}</button>` +
        `<button class="mini-btn danger" data-act="del" title="Удалить">${ICONS.trash}</button>` +
        `</span>`;
      $('[data-act="edit"]', card).addEventListener('click', (e) => {
        e.stopPropagation();
        startTaskRename(card, t);
      });
      $('[data-act="cmt"]', card).addEventListener('click', (e) => {
        e.stopPropagation();
        startTaskComment(card, t);
      });
      $('[data-act="del"]', card).addEventListener('click', (e) => {
        e.stopPropagation();
        const arr = PROJECTS[currentProject].tasks;
        arr.splice(arr.indexOf(t), 1);
        renderTasks();
        renderProjectList();
        toast(`Задача удалена: ${t.t}`);
      });
      card.addEventListener('dragstart', (e) => {
        e.dataTransfer.setData('text/plain', t.t);
        e.dataTransfer.effectAllowed = 'move';
        card.classList.add('dragging');
      });
      card.addEventListener('dragend', () => card.classList.remove('dragging'));
      card.addEventListener('click', () => toast('Задача: ' + t.t + ' (карандаш — редактирование, корзина — удаление)'));
      col.appendChild(card);
    });
  });
}

function startTaskRename(card, task) {
  const title = $('.t', card);
  const input = document.createElement('input');
  input.className = 'rename-input';
  input.value = task.t;
  title.replaceWith(input);
  input.focus();
  input.select();
  const commit = () => {
    const next = input.value.trim();
    if (next) task.t = next;
    renderTasks();
  };
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') commit();
    if (e.key === 'Escape') renderTasks();
  });
  input.addEventListener('blur', commit);
  input.addEventListener('click', (e) => e.stopPropagation());
  input.addEventListener('mousedown', (e) => e.stopPropagation());
}

function startTaskComment(card, task) {
  let target = $('.c', card);
  if (!target) {
    target = document.createElement('div');
    target.className = 'c';
    $('.m', card).after(target);
  }
  const input = document.createElement('input');
  input.className = 'rename-input';
  input.placeholder = 'Комментарий…';
  input.value = task.c || '';
  target.replaceWith(input);
  input.focus();
  const commit = () => {
    const next = input.value.trim();
    if (next) task.c = next; else delete task.c;
    renderTasks();
  };
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') commit();
    if (e.key === 'Escape') renderTasks();
  });
  input.addEventListener('blur', commit);
  input.addEventListener('click', (e) => e.stopPropagation());
  input.addEventListener('mousedown', (e) => e.stopPropagation());
}

// drag&drop задач между колонками
$$('.task-col').forEach((col) => {
  const list = $('.task-list', col);
  col.addEventListener('dragover', (e) => {
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
    col.classList.add('drop-over');
  });
  col.addEventListener('dragleave', () => col.classList.remove('drop-over'));
  col.addEventListener('drop', (e) => {
    e.preventDefault();
    col.classList.remove('drop-over');
    const title = e.dataTransfer.getData('text/plain');
    const task = PROJECTS[currentProject].tasks.find((t) => t.t === title);
    const target = list.dataset.status;
    if (task && task.s !== target) {
      task.s = target;
      renderTasks();
      renderProjectList();
      toast(`«${task.t}» → ${target === 'new' ? 'Новые' : target === 'progress' ? 'В работе' : 'Готово'}`);
    }
  });
});

// ---------- Чат проекта ----------

function renderProjectChat() {
  const el = $('#projectChat');
  el.innerHTML = '';
  PROJECTS[currentProject].chat.forEach((m) => el.appendChild(buildMsg(m)));
  el.scrollTop = el.scrollHeight;
}

function buildMsg(m) {
  const div = document.createElement('div');
  div.className = 'msg ' + m.who;
  let steps = '';
  if (m.steps) {
    steps = `<div class="tool-steps">` + m.steps.map((s) => `<div class="tool-step"><span class="dot"></span>${s}</div>`).join('') + `</div>`;
  }
  let atts = '';
  (m.attachments || []).forEach((a) => {
    if (a.kind === 'image') {
      atts += `<div class="msg-attachment"><img src="${a.url}" alt="${a.name}"><div class="cap">${ICONS.file}${a.name}</div></div>`;
    } else {
      atts += `<div class="msg-attachment"><div class="cap">${ICONS.file}${a.name}</div></div>`;
    }
  });
  div.innerHTML = `<span class="who">${m.who === 'user' ? 'Ты' : 'Агент'}</span>${steps}<div class="bubble">${m.text}</div>${atts}`;
  return div;
}

// ---------- Вложения в чатах ----------

const pending = { project: [], harness: [] };

function setupAttach(chatKey, btnId, inputId, barId) {
  const btn = $(btnId);
  const input = $(inputId);
  const bar = $(barId);
  btn.addEventListener('click', () => input.click());
  input.addEventListener('change', () => {
    for (const f of input.files) {
      const isImg = f.type.startsWith('image/');
      pending[chatKey].push({
        kind: isImg ? 'image' : 'file',
        name: f.name,
        url: isImg ? URL.createObjectURL(f) : null,
      });
    }
    input.value = '';
    renderAttachBar(chatKey, barId);
  });
  const zone = btn.closest('.chat') || btn.closest('.center');
  zone.addEventListener('dragover', (e) => e.preventDefault());
  zone.addEventListener('drop', (e) => {
    e.preventDefault();
    for (const f of e.dataTransfer.files) {
      const isImg = f.type.startsWith('image/');
      pending[chatKey].push({
        kind: isImg ? 'image' : 'file',
        name: f.name,
        url: isImg ? URL.createObjectURL(f) : null,
      });
    }
    renderAttachBar(chatKey, barId);
  });
  renderAttachBar(chatKey, bar);
}

function renderAttachBar(chatKey, barId) {
  const bar = typeof barId === 'string' ? $(barId) : barId;
  bar.innerHTML = '';
  const items = pending[chatKey];
  bar.hidden = items.length === 0;
  items.forEach((a, i) => {
    const chip = document.createElement('div');
    chip.className = 'attach-chip';
    chip.innerHTML =
      (a.kind === 'image' ? `<img src="${a.url}" alt="">` : ICONS.file) +
      `<span class="nm">${a.name}</span>` +
      `<button class="x" title="Убрать">${'<svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M18 6 6 18M6 6l12 12"/></svg>'}</button>`;
    chip.querySelector('.x').addEventListener('click', () => {
      pending[chatKey].splice(i, 1);
      renderAttachBar(chatKey, bar);
    });
    bar.appendChild(chip);
  });
}

setupAttach('project', '#projectAttachBtn', '#projectFileInput', '#projectAttachBar');
setupAttach('harness', '#harnessAttachBtn', '#harnessFileInput', '#harnessAttachBar');

function sendProjectChat() {
  const input = $('#projectChatInput');
  const text = input.value.trim();
  const atts = pending.project.splice(0);
  if (!text && !atts.length) return;
  input.value = '';
  input.style.height = '';
  renderAttachBar('project', '#projectAttachBar');
  PROJECTS[currentProject].chat.push({ who: 'user', text, attachments: atts });
  renderProjectChat();
  const names = atts.map((a) => `«${a.name}»`).join(', ');
  setTimeout(() => {
    PROJECTS[currentProject].chat.push({
      who: 'agent',
      steps: atts.length
        ? ['получено вложение: ' + names, 'сохранение в ' + currentProject + '/Входящие/', 'запись в INBOX.md', 'поиск по индексу проекта']
        : ['поиск по индексу проекта', 'чтение файлов'],
      text: atts.length
        ? `Получил ${names}. Сохранил в папку «Входящие» проекта и записал в INBOX.md. Картинки разобрал (зрение), документы — извлёк текст. Это моковый ответ прототипа.`
        : `Нашёл по проекту «${currentProject}»: релевантных фрагментов — 3. Кратко: договор подписан, открытые задачи — в колонке «Задачи». Это моковый ответ прототипа.`,
    });
    renderProjectChat();
  }, 500);
}

$('#projectChatSend').addEventListener('click', sendProjectChat);
$('#projectChatInput').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendProjectChat(); }
});
$$('.chat-input textarea').forEach((ta) => {
  ta.addEventListener('input', () => {
    ta.style.height = 'auto';
    ta.style.height = Math.min(ta.scrollHeight, 200) + 'px';
  });
});
[['#projectChatClear', '#projectChatInput', 'project', '#projectAttachBar'],
 ['#harnessChatClear', '#harnessChatInput', 'harness', '#harnessAttachBar']].forEach(([btn, ta, kind, bar]) => {
  $(btn).addEventListener('click', () => {
    const el = $(ta);
    el.value = '';
    el.style.height = '';
    pending[kind].splice(0);
    renderAttachBar(kind, bar);
    el.focus();
  });
});

// ---------- Харнес ----------

let currentSession = 's1';

function renderSessions() {
  const el = $('#sessionList');
  el.innerHTML = '';
  SESSIONS.forEach((s) => {
    const btn = document.createElement('button');
    btn.className = 'session-item' + (s.id === currentSession ? ' active' : '');
    btn.innerHTML = `<span class="name">${s.name}</span><span class="meta">${s.meta}</span>`;
    btn.addEventListener('click', () => {
      currentSession = s.id;
      $('#sessionTitle').textContent = s.name;
      $('#sessionBadge').textContent = s.meta.split(' · ').pop();
      renderSessions();
      renderHarnessChat();
    });
    el.appendChild(btn);
  });
}

function renderHarnessChat() {
  const el = $('#harnessChat');
  el.innerHTML = '';
  (HARNESS_CHATS[currentSession] || []).forEach((m) => el.appendChild(buildMsg(m)));
  el.scrollTop = el.scrollHeight;
}

function sendHarnessChat() {
  const input = $('#harnessChatInput');
  const text = input.value.trim();
  const atts = pending.harness.splice(0);
  if (!text && !atts.length) return;
  input.value = '';
  input.style.height = '';
  renderAttachBar('harness', '#harnessAttachBar');
  (HARNESS_CHATS[currentSession] ||= []).push({ who: 'user', text, attachments: atts });
  renderHarnessChat();
  const names = atts.map((a) => `«${a.name}»`).join(', ');
  setTimeout(() => {
    HARNESS_CHATS[currentSession].push({
      who: 'agent',
      steps: atts.length
        ? ['получено вложение: ' + names, 'сохранение в выбранную папку', 'разбор содержимого']
        : ['браузер (CDP): открыт headless Chrome', 'bash: выполнена команда'],
      text: atts.length
        ? `Принял ${names}. Сохраню по указанному пути, разберу содержимое (картинки — через зрение модели, документы — извлечение текста). Моковый ответ харнес-агента.`
        : 'Моковый ответ харнес-агента. В реальном приложении здесь будут tool-calls с подтверждениями опасных операций.',
    });
    renderHarnessChat();
  }, 600);
}

$('#harnessChatSend').addEventListener('click', sendHarnessChat);
$('#harnessChatInput').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendHarnessChat(); }
});

function renderSchedules() {
  const el = $('#scheduleList');
  el.innerHTML = '';
  SCHEDULES.forEach((s) => {
    const item = document.createElement('div');
    item.className = 'schedule-item';
    item.innerHTML = `<div class="info"><div class="t">${s.t}</div><div class="c">${s.c}</div></div>
      <label class="switch"><input type="checkbox" ${s.on ? 'checked' : ''}><span class="track"></span></label>`;
    item.querySelector('input').addEventListener('change', (e) => toast(`Расписание «${s.t}» ${e.target.checked ? 'включено' : 'выключено'}`));
    el.appendChild(item);
  });
}

function renderMemory() {
  const el = $('#memoryList');
  el.innerHTML = '';
  MEMORY.forEach((m) => {
    const div = document.createElement('div');
    div.className = 'memory-item';
    div.textContent = m;
    el.appendChild(div);
  });
}

// ---------- Настройки ----------

function renderSettingsLists() {
  const sl = $('#skillList');
  sl.innerHTML = '';
  SKILLS.forEach((s) => {
    const row = document.createElement('div');
    row.className = 'set-item';
    row.innerHTML = `<span>${s.name}</span><span class="kind">${s.kind}</span><span class="spacer"></span><span class="switch${s.on ? ' on' : ''}"></span>`;
    $('.switch', row).addEventListener('click', () => { s.on = !s.on; $('.switch', row).classList.toggle('on', s.on); });
    sl.appendChild(row);
  });
  const ml = $('#mcpList');
  ml.innerHTML = '';
  MCP_SERVERS.forEach((m) => {
    const row = document.createElement('div');
    row.className = 'set-item';
    row.innerHTML = `<span>${m.name}</span><span class="kind">${m.kind}</span><span class="status-ok">${m.status}</span><span class="spacer"></span><span class="switch${m.on ? ' on' : ''}"></span>`;
    $('.switch', row).addEventListener('click', () => { m.on = !m.on; $('.switch', row).classList.toggle('on', m.on); });
    ml.appendChild(row);
  });
  const mem = $('#memorySettings');
  mem.innerHTML = '';
  MEMORY.forEach((m) => {
    const row = document.createElement('div');
    row.className = 'set-item';
    row.innerHTML = `<span>${m}</span><span class="spacer"></span><button class="mini-btn danger" title="Удалить">${ICONS.trash}</button>`;
    $('[data-act], button', row).addEventListener('click', () => toast('Запись памяти удалена'));
    mem.appendChild(row);
  });
}

function openSettings(sec) {
  $('#settingsModal').hidden = false;
  selectSettingsSec(sec || 'llm');
  renderSettingsLists();
}

function selectSettingsSec(sec) {
  $$('.settings-tab').forEach((b) => b.classList.toggle('active', b.dataset.sec === sec));
  $$('.settings-sec').forEach((s) => s.classList.toggle('active', s.dataset.sec === sec));
}

$('.topbar-right .icon-btn').addEventListener('click', () => openSettings());
$('#settingsClose').addEventListener('click', () => { $('#settingsModal').hidden = true; });
$('#settingsModal').addEventListener('click', (e) => { if (e.target.id === 'settingsModal') $('#settingsModal').hidden = true; });
$$('.settings-tab').forEach((b) => b.addEventListener('click', () => selectSettingsSec(b.dataset.sec)));
$$('.switch-row .switch').forEach((sw) => sw.addEventListener('click', () => sw.classList.toggle('on')));
$('#memoryOpen').addEventListener('click', () => openSettings('memory'));
$('#llmCheck').addEventListener('click', () => toast('Подключение: OK, модель отвечает'));
$('#reindexBtn').addEventListener('click', () => toast('Переиндексация запущена (все проекты)'));
$('#skillImport').addEventListener('click', () => toast('Импорт скилла: папка / .zip / git-репозиторий'));
$('#mcpAdd').addEventListener('click', () => toast('MCP-сервер добавляется в Settings/config/mcp.json'));
$('#memoryAdd').addEventListener('click', () => toast('Новая запись глобальной памяти'));
$('#tmpClearBtn').addEventListener('click', () => toast('Harness/tmp/ очищен'));
$('#settingsBackupBtn').addEventListener('click', () => toast('Резервная копия: архив .zip с БД и файлами проектов'));

// ---------- Кнопки-заглушки ----------

$('#addProject').addEventListener('click', () => toast('Новый проект: имя → создадутся папка, datas.md и tasks/'));
$('#addSession').addEventListener('click', () => toast('Новая сессия харнес-агента'));
$('#backupBtn').addEventListener('click', () => toast('Резервная копия: архив .zip с БД и файлами проектов'));

// ---------- Старт ----------

function renderAll() {
  renderProjectList();
  renderTree();
  renderTasks();
  renderProjectChat();
}

renderAll();
renderSessions();
renderHarnessChat();
renderSchedules();
renderMemory();

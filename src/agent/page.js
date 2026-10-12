// The page half of the agent tools (src/agent.rs). Runs in a private script
// world (agent::WORLD), so the page can neither see these helpers nor replace
// them; it shares only the DOM. Installed once per document on first use;
// every call is `<this file>; __cceAgent.<fn>(...)`.
//
// Refs: every element a tree or a find reports gets a `ref_N`, stable for the
// element's life in this document, held weakly so a ref never keeps a removed
// element alive. A ref whose element is gone says so, and the agent reads the
// page again.
(() => {
  if (globalThis.__cceAgent) return;

  const refs = new Map();
  const ids = new WeakMap();
  let next = 1;
  const refOf = (el) => {
    let r = ids.get(el);
    if (!r) {
      r = 'ref_' + next++;
      ids.set(el, r);
      refs.set(r, new WeakRef(el));
    }
    return r;
  };
  const byRef = (r) => {
    const el = refs.get(r)?.deref();
    if (!el || !el.isConnected) throw new Error(`${r} names nothing on the page now (refs last only as long as their element and the page); read the page again`);
    return el;
  };

  const clean = (s, max) => {
    s = String(s ?? '').replace(/\s+/g, ' ').trim();
    return s.length > max ? s.slice(0, max - 1) + '…' : s;
  };
  const quote = (s) => '"' + s.replace(/\\/g, '\\\\').replace(/"/g, '\\"') + '"';

  const SKIP = new Set(['script', 'style', 'noscript', 'template', 'head', 'meta', 'link', 'title', 'base']);
  const TAG_ROLE = {
    button: 'button', summary: 'button', select: 'combobox', textarea: 'textbox', option: 'option',
    nav: 'navigation', main: 'main', header: 'banner', footer: 'contentinfo', aside: 'complementary',
    form: 'form', dialog: 'dialog', table: 'table', ul: 'list', ol: 'list', menu: 'list', li: 'listitem',
    tr: 'row', td: 'cell', th: 'columnheader', img: 'img', p: 'paragraph', iframe: 'iframe', frame: 'iframe',
    video: 'video', audio: 'audio', label: 'label', fieldset: 'group', details: 'group', article: 'article',
    figure: 'figure', figcaption: 'caption', caption: 'caption', hr: 'separator', progress: 'progressbar',
    meter: 'meter', blockquote: 'blockquote', pre: 'code', dt: 'term', dd: 'definition', legend: 'legend',
    h1: 'heading', h2: 'heading', h3: 'heading', h4: 'heading', h5: 'heading', h6: 'heading',
  };
  const INPUT_ROLE = {
    checkbox: 'checkbox', radio: 'radio', range: 'slider', number: 'spinbutton', search: 'searchbox',
    button: 'button', submit: 'button', reset: 'button', image: 'button', file: 'button', color: 'button',
  };
  const INTERACTIVE = new Set([
    'button', 'link', 'checkbox', 'radio', 'textbox', 'searchbox', 'combobox', 'listbox', 'option',
    'menuitem', 'menuitemcheckbox', 'menuitemradio', 'tab', 'switch', 'slider', 'spinbutton', 'treeitem',
  ]);
  // Roles whose name is their text, and whose text is shown on the line
  // instead of as children when nothing inside them can be acted on.
  const NAME_FROM_CONTENT = new Set([
    'button', 'link', 'heading', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'option',
    'cell', 'columnheader', 'treeitem', 'switch', 'checkbox', 'radio', 'label', 'caption', 'legend', 'term',
  ]);
  const TEXT_BLOCK = new Set([
    'paragraph', 'heading', 'listitem', 'cell', 'columnheader', 'label', 'caption', 'blockquote', 'code',
    'term', 'definition', 'legend',
  ]);
  const INTERACTIVE_SEL = 'a[href],button,input:not([type=hidden]),select,textarea,summary,[contenteditable=""],'
    + '[contenteditable=true],[onclick],[tabindex]:not([tabindex="-1"]),[role=button],[role=link],[role=checkbox],'
    + '[role=radio],[role=tab],[role=menuitem],[role=switch],[role=option],[role=combobox],[role=textbox]';

  const style = (el) => el.ownerDocument.defaultView.getComputedStyle(el);

  const visible = (el) => {
    if (el.getAttribute('aria-hidden') === 'true') return false;
    if (el.checkVisibility) {
      if (el.checkVisibility({ checkVisibilityCSS: true })) return true;
      // No box of its own, but its children have theirs.
      return style(el).display === 'contents';
    }
    const s = style(el);
    return s.display !== 'none' && s.visibility !== 'hidden';
  };

  const roleOf = (el) => {
    const explicit = el.getAttribute('role');
    if (explicit) return explicit.trim().split(/\s+/)[0];
    const tag = el.localName;
    if (tag === 'a' || tag === 'area') return el.hasAttribute('href') ? 'link' : 'generic';
    if (tag === 'input') return INPUT_ROLE[(el.type || 'text').toLowerCase()] || 'textbox';
    if (tag === 'select' && (el.multiple || el.size > 1)) return 'listbox';
    if (tag === 'section') return el.hasAttribute('aria-label') || el.hasAttribute('aria-labelledby') ? 'region' : 'generic';
    if (tag === 'svg') return el.querySelector(':scope > title') || el.hasAttribute('aria-label') ? 'img' : 'generic';
    if (el.isContentEditable && el.hasAttribute('contenteditable')) return 'textbox';
    return TAG_ROLE[tag] || 'generic';
  };

  // Clickable without saying so: a listener the page attached (which no
  // attribute shows) usually comes with a pointer cursor, so a pointer that
  // starts here rather than inheriting from the parent is the tell.
  const clickable = (el) => {
    if (el.hasAttribute('onclick')) return true;
    if (el.hasAttribute('tabindex') && el.tabIndex >= 0) return true;
    if (style(el).cursor !== 'pointer') return false;
    const p = el.parentElement;
    return !p || style(p).cursor !== 'pointer';
  };

  const interactive = (el, role) => {
    if (INTERACTIVE.has(role)) return true;
    if (el.isContentEditable && el.hasAttribute('contenteditable')) return true;
    return role === 'generic' && clickable(el);
  };

  const nameOf = (el, role) => {
    const tag = el.localName;
    let n = el.getAttribute('aria-label');
    if (!n) {
      const by = el.getAttribute('aria-labelledby');
      if (by) n = by.split(/\s+/).map((id) => el.ownerDocument.getElementById(id)?.innerText || '').join(' ');
    }
    if (!n && el.labels?.length) n = [...el.labels].map((l) => l.innerText).join(' ');
    if (!n && (tag === 'img' || tag === 'area')) n = el.alt;
    if (!n && tag === 'input' && ['button', 'submit', 'reset', 'image'].includes(el.type)) n = el.value || el.alt;
    if (!n && tag === 'svg') n = el.querySelector(':scope > title')?.textContent;
    if (!n && tag === 'iframe') n = el.title;
    if (!n && (NAME_FROM_CONTENT.has(role) || role === 'generic')) {
      n = el.innerText;
      // A link or button that is only a picture is named by the picture.
      if (!n.trim()) {
        const pic = el.querySelector('img[alt]:not([alt=""]), [aria-label], svg title');
        n = pic?.alt || pic?.getAttribute?.('aria-label') || pic?.textContent || '';
      }
    }
    if (!n) n = el.getAttribute('title') || el.getAttribute('placeholder') || '';
    return clean(n, 100);
  };

  const states = (el, role, acts) => {
    const tag = el.localName;
    const out = [];
    if (role === 'heading') out.push('level=' + (tag[0] === 'h' ? tag[1] : el.getAttribute('aria-level') || '2'));
    if (role === 'link') {
      const href = el.getAttribute('href');
      if (href) out.push('href=' + quote(clean(href, 120)));
    }
    if (tag === 'input' || tag === 'textarea') {
      const type = (el.type || 'text').toLowerCase();
      if (type === 'checkbox' || type === 'radio') out.push(el.checked ? 'checked' : 'unchecked');
      else if (!INPUT_ROLE[type] || type === 'search' || type === 'number' || type === 'range') {
        if (!['text', 'search', 'number', 'range'].includes(type) && tag === 'input') out.push('type=' + type);
        out.push('value=' + quote(type === 'password' ? '•'.repeat(Math.min(el.value.length, 8)) : clean(el.value, 100)));
        if (el.placeholder && nameOf(el, role) !== clean(el.placeholder, 100)) out.push('placeholder=' + quote(clean(el.placeholder, 60)));
      }
    }
    if (tag === 'select') {
      const picked = [...el.selectedOptions].map((o) => clean(o.text, 60)).join(', ');
      out.push('value=' + quote(picked));
    }
    if (el.isContentEditable && el.hasAttribute('contenteditable') && tag !== 'input' && tag !== 'textarea') {
      out.push('value=' + quote(clean(el.innerText, 100)));
    }
    if (tag === 'option' && el.selected) out.push('selected');
    for (const a of ['checked', 'pressed', 'selected', 'expanded']) {
      const v = el.getAttribute('aria-' + a);
      if (v === 'true' || v === 'false' || v === 'mixed') out.push(a + '=' + v);
    }
    if (el.disabled || el.getAttribute('aria-disabled') === 'true') out.push('disabled');
    if (el.required) out.push('required');
    if (el === el.ownerDocument.activeElement && el !== el.ownerDocument.body) out.push('focused');
    if (role === 'generic' && acts) out.push('clickable');
    return out;
  };

  // What is drawn inside: an open shadow root's tree, a slot's assigned
  // nodes, a same-origin frame's document.
  const kids = (el) => {
    if (el.shadowRoot) return el.shadowRoot.childNodes;
    if (el.localName === 'slot') {
      const a = el.assignedNodes({ flatten: true });
      return a.length ? a : el.childNodes;
    }
    if (el.localName === 'iframe' || el.localName === 'frame') {
      try {
        const d = el.contentDocument;
        if (d?.body) return [d.body];
      } catch {}
      return [];
    }
    return el.childNodes;
  };

  // The page as lines: `role "name" [ref_N] states`, indented by nesting.
  // Containers that say nothing (role generic, no name) are not shown; their
  // children move up a level.
  const walk = (root, opts) => {
    const lines = [];
    let chars = 0;
    let truncated = false;
    const emit = (depth, line, ref) => {
      if (chars >= opts.maxChars) return (truncated = true), false;
      const text = '  '.repeat(depth) + '- ' + line;
      chars += text.length + 1;
      lines.push({ text, ref });
      return true;
    };
    const visit = (node, depth) => {
      if (truncated) return;
      if (node.nodeType === Node.TEXT_NODE) {
        if (opts.interactive) return;
        const t = clean(node.data, 200);
        if (t) emit(depth, 'text ' + quote(t));
        return;
      }
      if (node.nodeType !== Node.ELEMENT_NODE && node.nodeType !== Node.DOCUMENT_FRAGMENT_NODE) return;
      if (node.nodeType === Node.ELEMENT_NODE) {
        const el = node;
        if (SKIP.has(el.localName) || !visible(el)) return;
        const role = roleOf(el);
        if (role === 'none' || role === 'presentation') {
          for (const k of kids(el)) visit(k, depth);
          return;
        }
        const acts = interactive(el, role);
        let shown;
        if (opts.interactive) shown = acts;
        else shown = acts || role !== 'generic' || !!el.getAttribute('aria-label');
        if (shown) {
          const name = nameOf(el, role);
          if (role === 'img' && !name && !acts && !opts.interactive) return;
          const r = refOf(el);
          const parts = [role === 'generic' ? 'generic' : role];
          if (name) parts.push(quote(name));
          parts.push('[' + r + ']');
          parts.push(...states(el, role, acts));
          if (!emit(depth, parts.join(' '), r)) return;
          // A block of plain text has been said by its name; a field or a
          // list of options has no children worth more lines (a select's
          // options are, so those go on).
          if (role === 'img' || el.localName === 'input' || el.localName === 'textarea') return;
          if (NAME_FROM_CONTENT.has(role) && role !== 'cell' && role !== 'columnheader') {
            if (!el.querySelector(INTERACTIVE_SEL)) return;
          }
          if (TEXT_BLOCK.has(role) && !el.querySelector(INTERACTIVE_SEL) && !opts.interactive) {
            // Its text in one line rather than a line per run.
            const t = clean(el.innerText, 300);
            if (t && t !== name) emit(depth + 1, 'text ' + quote(t));
            return;
          }
          if (depth + 1 > opts.depth) return;
          for (const k of kids(el)) visit(k, depth + 1);
          return;
        }
      }
      for (const k of kids(node)) visit(k, depth);
    };
    visit(root, 0);
    return { lines, truncated };
  };

  const doc = () => document.body || document.documentElement;

  // Where an element is, in the top viewport's CSS pixels: its own rect plus
  // the offset of every same-origin frame it sits in.
  const place = (el) => {
    const rects = el.getClientRects();
    const r = rects.length ? rects[0] : el.getBoundingClientRect();
    let x = r.left, y = r.top;
    let win = el.ownerDocument.defaultView;
    while (win && win.frameElement) {
      const f = win.frameElement;
      const fr = f.getBoundingClientRect();
      x += fr.left + f.clientLeft;
      y += fr.top + f.clientTop;
      win = f.ownerDocument.defaultView;
    }
    return { x, y, w: r.width, h: r.height };
  };

  globalThis.__cceAgent = {
    tree(o) {
      const root = o.ref ? byRef(o.ref) : doc();
      const { lines, truncated } = walk(root, {
        interactive: o.filter === 'interactive',
        depth: o.depth ?? 40,
        maxChars: o.maxChars ?? 50000,
      });
      let out = lines.map((l) => l.text).join('\n');
      if (truncated) out += '\n[truncated at ' + (o.maxChars ?? 50000) + ' characters: pass ref_id to read one part, or a larger max_chars]';
      return JSON.stringify({
        url: location.href,
        title: document.title,
        viewport: [innerWidth, innerHeight],
        scroll: [Math.round(scrollX), Math.round(scrollY)],
        page: [document.documentElement.scrollWidth, document.documentElement.scrollHeight],
        tree: out || '(nothing visible)',
      });
    },

    find(query) {
      const q = String(query).toLowerCase().trim();
      const words = q.split(/\s+/).filter(Boolean);
      const { lines } = walk(doc(), { interactive: false, depth: 1000, maxChars: Infinity });
      const hits = [];
      for (const l of lines) {
        if (!l.ref) continue;
        const t = l.text.trim().toLowerCase();
        if (t.includes(q) || words.every((w) => t.includes(w))) {
          const el = refs.get(l.ref)?.deref();
          const p = el ? place(el) : null;
          const where = p ? ` at (${Math.round(p.x + p.w / 2)}, ${Math.round(p.y + p.h / 2)})` : '';
          hits.push(l.text.trim().replace(/^- /, '') + where);
          if (hits.length >= 20) break;
        }
      }
      return JSON.stringify(hits);
    },

    text(max) {
      const main = document.querySelector('main, [role=main], article');
      const from = main && main.innerText.trim().length > 200 ? main : doc();
      let t = (from.innerText || '').replace(/\n{3,}/g, '\n\n').trim();
      let truncated = false;
      if (t.length > max) {
        t = t.slice(0, max);
        truncated = true;
      }
      return JSON.stringify({ url: location.href, title: document.title, text: t, truncated });
    },

    // Where to click a ref: brought into view first if it is not, then the
    // middle of its first box.
    point(r) {
      const el = byRef(r);
      let p = place(el);
      const off = p.y + p.h < 0 || p.y > innerHeight || p.x + p.w < 0 || p.x > innerWidth;
      if (off) {
        el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
        p = place(el);
      }
      if (p.w === 0 && p.h === 0) throw new Error(`${r} has no box on screen to click`);
      const x = p.x + p.w / 2, y = p.y + p.h / 2;
      const at = document.elementFromPoint(x, y);
      const covered = at && at !== el && !el.contains(at) && !at.contains(el) && el.ownerDocument === document;
      return JSON.stringify({ x, y, covered: covered ? clean(at.outerHTML.split('>')[0] + '>', 120) : null });
    },

    scrollTo(r) {
      byRef(r).scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
      return JSON.stringify(true);
    },

    // Set a control's value the way a person's input would leave it: the
    // events a framework listens for fire, and a checkbox is clicked rather
    // than assigned so its handlers run.
    input(r, value) {
      const el = byRef(r);
      const tag = el.localName;
      const fire = (type) => el.dispatchEvent(new Event(type, { bubbles: true }));
      if (tag === 'select') {
        const want = String(value);
        const opts = [...el.options];
        const opt = opts.find((o) => o.value === want)
          || opts.find((o) => clean(o.text, 1000) === want)
          || opts.find((o) => clean(o.text, 1000).toLowerCase().includes(want.toLowerCase()));
        if (!opt) throw new Error(`no option ${quote(want)}; the options are ${opts.map((o) => quote(clean(o.text, 60))).join(', ')}`);
        el.value = opt.value;
        opt.selected = true;
        fire('input');
        fire('change');
        return JSON.stringify(`selected ${quote(clean(opt.text, 60))}`);
      }
      const type = (el.type || '').toLowerCase();
      if (tag === 'input' && (type === 'checkbox' || type === 'radio')) {
        const on = value === true || value === 'true' || value === 'on' || value === 1 || value === '1';
        if (el.checked !== on) el.click();
        return JSON.stringify(el.checked ? 'checked' : 'unchecked');
      }
      if (tag === 'input' || tag === 'textarea') {
        el.focus();
        const proto = tag === 'input' ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
        Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, String(value));
        fire('input');
        fire('change');
        return JSON.stringify(`value is ${quote(clean(el.value, 100))}`);
      }
      if (el.isContentEditable) {
        el.focus();
        const sel = el.ownerDocument.getSelection();
        sel.selectAllChildren(el);
        el.ownerDocument.execCommand('insertText', false, String(value));
        return JSON.stringify(`text is ${quote(clean(el.innerText, 100))}`);
      }
      throw new Error(`${r} is a ${tag}, which takes no value; click it instead`);
    },
  };
})();

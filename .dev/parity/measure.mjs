// Measure ChipFlow elements and compare them with KanbanFlow golden values.

const SIDES = ['Top', 'Right', 'Bottom', 'Left'];

// Runs in the page: everything a comparison can ask for.
function measureInPage(el, range) {
  let r = el.getBoundingClientRect();
  if (range) {
    // Union of the element's text-node boxes (the ink-line box, not the
    // element box); whitespace-only nodes are skipped.
    const w = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    let u = null;
    for (let n = w.nextNode(); n; n = w.nextNode()) {
      if (!n.nodeValue.trim()) continue;
      const rg = document.createRange();
      rg.selectNodeContents(n);
      for (const q of rg.getClientRects()) {
        u = u ? { l: Math.min(u.l, q.left), t: Math.min(u.t, q.top), r: Math.max(u.r, q.right), b: Math.max(u.b, q.bottom) } : { l: q.left, t: q.top, r: q.right, b: q.bottom };
      }
    }
    if (u) r = { left: u.l, top: u.t, width: u.r - u.l, height: u.b - u.t };
  }
  const cs = getComputedStyle(el);
  const side = (prefix, suffix = '') => ['Top', 'Right', 'Bottom', 'Left'].map((s) => cs[prefix + s + suffix]);
  const border = ['Top', 'Right', 'Bottom', 'Left'].map((s) => ({
    w: cs[`border${s}Width`], style: cs[`border${s}Style`], color: cs[`border${s}Color`],
  }));
  const shown = r.width > 0 && r.height > 0 && cs.visibility !== 'hidden' && cs.display !== 'none';
  return {
    shown,
    box: { x: r.left, y: r.top, w: r.width, h: r.height },
    scrollW: el.scrollWidth,
    clientW: el.clientWidth,
    c: cs.color,
    bg: cs.backgroundColor,
    fs: cs.fontSize,
    fw: cs.fontWeight,
    fst: cs.fontStyle,
    lh: cs.lineHeight,
    br: cs.borderTopLeftRadius,
    o: cs.opacity,
    p: side('padding'),
    m: side('margin'),
    b: border,
    text: (el.innerText || el.value || el.getAttribute('placeholder') || '').replace(/\s+/g, ' ').trim(),
  };
}

// sel is a Playwright selector (CSS plus :has-text and friends).
export async function measure(page, sel, nth = 0, range = false) {
  const loc = page.locator(sel);
  const count = await loc.count();
  if (nth >= count) return { count };
  return { count, ...(await loc.nth(nth).evaluate(measureInPage, range)) };
}

// "#rrggbb", "#rrggbb/a" or "rgb()/rgba()" to [r,g,b,a].
export function parseColor(s) {
  if (!s) return null;
  s = s.trim();
  let m = /^#([0-9a-f]{6})(?:\/([0-9.]+))?$/i.exec(s);
  if (m) {
    const n = parseInt(m[1], 16);
    return [(n >> 16) & 255, (n >> 8) & 255, n & 255, m[2] === undefined ? 1 : parseFloat(m[2])];
  }
  m = /^rgba?\(([^)]+)\)$/.exec(s);
  if (m) {
    const p = m[1].split(/[ ,/]+/).filter(Boolean).map(parseFloat);
    return [p[0], p[1], p[2], p.length > 3 ? p[3] : 1];
  }
  if (s === 'transparent') return [0, 0, 0, 0];
  return null;
}

const hex = (c) =>
  c ? `#${c.slice(0, 3).map((v) => Math.round(v).toString(16).padStart(2, '0')).join('')}${c[3] === 1 ? '' : '/' + +c[3].toFixed(3)}` : String(c);

function sameColor(exp, act, tol) {
  if (!exp || !act) return false;
  // A fully transparent colour matches any rgb.
  if (exp[3] === 0 && act[3] === 0) return true;
  return exp.slice(0, 3).every((v, i) => Math.abs(v - act[i]) <= tol.color) && Math.abs(exp[3] - act[3]) <= 0.02;
}

const px = (s) => (s === undefined || s === null ? NaN : parseFloat(s));

// Expand a CSS shorthand "a b c d" to four numbers.
function expand4(s) {
  const v = String(s).split(/\s+/).map(px);
  if (v.length === 1) return [v[0], v[0], v[0], v[0]];
  if (v.length === 2) return [v[0], v[1], v[0], v[1]];
  if (v.length === 3) return [v[0], v[1], v[2], v[1]];
  return v.slice(0, 4);
}

function parseBorderPart(part) {
  part = part.trim();
  if (/^0(px)?(\s+none)?$/.test(part) || part === 'none') return { w: 0 };
  const tokens = part.split(/\s+/);
  const out = { w: px(tokens[0]) };
  for (const t of tokens.slice(1)) {
    if (/^(solid|dashed|dotted|double|none)$/.test(t)) out.style = t;
    else out.color = parseColor(t === 'transparent' ? 'transparent' : t);
  }
  return out;
}

const fmtBorder = (b) =>
  b.map((s) => (px(s.w) === 0 || s.style === 'none' ? '0' : `${s.w} ${s.style} ${hex(parseColor(s.color))}`)).join('|');

function cmp(prop, expected, actualMeasure, tol, offset, over) {
  const rec = (pass, exp, act, t) => ({ prop, pass, expected: exp, actual: act, tol: t });
  const A = actualMeasure;
  if (['x', 'y', 'w', 'h', 'right', 'bottom'].includes(prop)) {
    const e = expected + (prop === 'x' || prop === 'right' ? offset.x : prop === 'y' || prop === 'bottom' ? offset.y : 0);
    const a = prop === 'right' ? A.box.x + A.box.w : prop === 'bottom' ? A.box.y + A.box.h : A.box[prop];
    const t = tol.px;
    return rec(Math.abs(e - a) <= t, e, +a.toFixed(2), t);
  }
  switch (prop) {
    case 'scrollW':
    case 'clientW': {
      const a = A[prop];
      return rec(Math.abs(expected - a) <= tol.px, expected, a, tol.px);
    }
    case 'c':
    case 'bg': {
      let e = parseColor(expected) ?? [0, 0, 0, 0];
      const a = parseColor(A[prop]);
      // A translucent golden colour is compared as rendered over the named
      // backdrop, since ChipFlow may paint the same pixels with an opaque colour.
      let t = tol;
      if (over?.[prop] && e[3] < 1 && a[3] === 1) {
        t = { ...tol, color: Math.max(tol.color, 1) };
        const bd = parseColor(over[prop]);
        e = [0, 1, 2].map((i) => Math.round(e[i] * e[3] + bd[i] * (1 - e[3]))).concat(1);
      }
      return rec(sameColor(e, a, t), hex(e), hex(a), t.color);
    }
    case 'fs': {
      const e = px(expected), a = px(A.fs);
      return rec(Math.abs(e - a) <= 0.1, e, a, 0.1);
    }
    case 'fw': {
      const e = String(expected), a = String(A.fw);
      return rec(e === a, e, a, 0);
    }
    case 'fst': {
      const e = expected || 'normal';
      return rec(e === A.fst, e, A.fst, 0);
    }
    case 'lh': {
      if (expected === 'normal' || A.lh === 'normal') return rec(expected === A.lh, expected, A.lh, 0);
      const e = px(expected), a = px(A.lh);
      return rec(Math.abs(e - a) <= tol.lh, e, a, tol.lh);
    }
    case 'o': {
      const e = parseFloat(expected ?? 1), a = parseFloat(A.o);
      return rec(Math.abs(e - a) <= 0.02, e, a, 0.02);
    }
    case 'br': {
      const e = expected ?? '0px';
      if (e === A.br) return rec(true, e, A.br, 0);
      if (e.endsWith('px') && A.br.endsWith('px')) return rec(Math.abs(px(e) - px(A.br)) <= 0.5, e, A.br, 0.5);
      return rec(false, e, A.br, 0);
    }
    case 'p':
    case 'm': {
      const e = expand4(expected ?? '0px');
      const a = A[prop].map(px);
      return rec(e.every((v, i) => Math.abs(v - a[i]) <= 0.5), e.join(' '), a.join(' '), 0.5);
    }
    case 'b': {
      const parts = (expected ?? '0').split('|');
      const exp = parts.length === 4 ? parts : [parts[0], parts[0], parts[0], parts[0]];
      let ok = true;
      exp.forEach((part, i) => {
        const e = parseBorderPart(part);
        const a = A.b[i];
        const aw = px(a.w);
        if (e.w === 0) {
          if (!(aw === 0 || a.style === 'none')) ok = false;
          return;
        }
        if (Math.abs(aw - e.w) > 0.5) ok = false;
        if (e.style && a.style !== e.style) ok = false;
        if (!e.style && a.style !== 'solid') ok = false;
        if (e.color && !sameColor(e.color, parseColor(a.color), tol)) ok = false;
      });
      return rec(ok, exp.map((s) => s.trim()).join('|'), fmtBorder(A.b), 0.5);
    }
    case 'text': {
      const norm = (s) => String(s).replace(/\s+/g, ' ').trim();
      const e = norm(expected), a = norm(A.text);
      // An empty golden text means "no text": prefix matching would pass anything.
      if (e === '') return rec(a === '', e, a.slice(0, 80), 0);
      return rec(a === e || a.startsWith(e) || a.toLowerCase() === e.toLowerCase(), e, a.slice(0, 80), 0);
    }
    default:
      return rec(false, expected, '(unsupported prop)', 0);
  }
}

// Golden element lookup: label equals key or its part before "(" does.
export function goldenElement(golden, key, gnth = 0) {
  const hits = (golden.els || []).filter((e) => e[0] === key || e[0].split('(')[0].trim() === key);
  const e = hits[gnth];
  if (!e) return null;
  const [, boxStr, style, text] = e;
  const n = (boxStr || '').split(',').map(Number);
  const box = n.length === 4 && n.every((v) => !Number.isNaN(v)) ? { x: n[0], y: n[1], w: n[2], h: n[3] } : null;
  return { box, style: style || {}, text: text ?? '' };
}

// Dotted path into a golden object ("columns.0.header.w"), for the responsive
// goldens, which are structured dumps instead of element lists.
function getPath(obj, dotted) {
  return dotted.split('.').reduce((o, k) => (o == null ? o : o[k]), obj);
}

const BOX_PROPS = ['x', 'y', 'w', 'h', 'right', 'bottom'];

// Run one comparison against the page; returns {id, pass, missing, props}.
export async function runComparison(page, c, golden, defaults) {
  const tol = { px: 1, color: 0, lh: 0.5, ...defaults, ...(c.tol || {}) };
  const offset = { x: 0, y: 0, ...(c.offset || {}) };
  const out = { id: c.id, key: c.key || null, sel: c.sel, props: [], pass: true };
  const g = c.key ? goldenElement(golden, c.key, c.gnth || 0) : null;
  if (c.key && !g && !c.expect && !c.path) {
    out.pass = false;
    out.error = `golden element '${c.key}' not found`;
    return out;
  }
  const m = await measure(page, c.sel, c.nth || 0, !!c.range);
  if (!m.box) {
    out.pass = false;
    out.error = `selector '${c.sel}' matched ${m.count} element(s), wanted index ${c.nth || 0}`;
    return out;
  }
  if (c.shown !== false && !m.shown) {
    out.pass = false;
    out.error = `selector '${c.sel}' is not visible`;
    return out;
  }
  for (const prop of c.props) {
    let expected;
    if (c.expect && prop in c.expect) expected = c.expect[prop];
    else if (c.path && prop in c.path) expected = getPath(golden, c.path[prop]);
    else if (BOX_PROPS.includes(prop)) {
      const b = g?.box;
      expected = b ? (prop === 'right' ? b.x + b.w : prop === 'bottom' ? b.y + b.h : b[prop]) : undefined;
    }
    else if (prop === 'text') expected = g?.text;
    else expected = g?.style?.[prop];
    if (expected === undefined && c.path && prop in c.path) {
      out.props.push({ prop, pass: false, expected: null, actual: null, tol: 0, note: `golden path ${c.path[prop]} missing` });
      out.pass = false;
      continue;
    }
    if (BOX_PROPS.includes(prop) && expected === undefined) {
      out.props.push({ prop, pass: false, expected: null, actual: null, tol: 0, note: 'golden has no box' });
      out.pass = false;
      continue;
    }
    const r = cmp(prop, expected, m, tol, offset, c.over);
    if (c.offset && (prop === 'x' || prop === 'y')) r.offset = offset[prop];
    out.props.push(r);
    if (!r.pass) out.pass = false;
  }
  return out;
}

export { SIDES };

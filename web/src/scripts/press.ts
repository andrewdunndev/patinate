// The press: mounts the one live plate (a `--web --transparent-bg`
// render of the fixture), maps a theme onto it, and prints the command
// that reproduces the proof with real flags only.
import type { Theme } from '../data/themes';

const SVG_NS = 'http://www.w3.org/2000/svg';
const PLATE = '/heatmap-web.svg';

// serde defaults from src/render/theme.rs, used when a theme omits a glow key.
const GLOW = { outer_ratio: 3.5, outer_alpha: 0.025, inner_ratio: 1.7, inner_alpha: 0.06 };
// compose.rs keeps the --web halos at this fraction of their opacity.
const WEB_GLOW = 0.5;
const TYPES = ['Ride', 'EBikeRide', 'Run', 'Walk', 'Hike'] as const;

type Crop = 'poster' | 'square' | 'wide';
const CROPS: Record<Crop, { label: string; vb: [number, number, number, number]; flags: string[] }> = {
  poster: { label: 'poster 3:4', vb: [0, 0, 1200, 1600], flags: [] },
  square: { label: 'square 1:1', vb: [0, 200, 1200, 1200], flags: ['--viewbox-width 1200 --viewbox-height 1200'] },
  wide: { label: 'wide 5:3', vb: [0, 440, 1200, 720], flags: ['--viewbox-width 1200 --viewbox-height 720 --fit cover'] },
};

export interface State {
  name: string;
  theme: Theme;
  edited: boolean;
  web: boolean;
  bloom: number;
  alpha: number;
  basemap: boolean;
  paper: boolean;
  anonymize: boolean;
  types: Set<string>;
  crop: Crop;
}

const $ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = document): T => {
  const el = root.querySelector<T>(sel);
  if (!el) throw new Error('missing ' + sel);
  return el;
};
const clone = <T>(v: T): T => JSON.parse(JSON.stringify(v)) as T;

function rgb(hex: string): [number, number, number] {
  const n = parseInt(hex.slice(1), 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}
function hex(c: number[]): string {
  return '#' + c.map((x) => Math.round(Math.max(0, Math.min(255, x))).toString(16).padStart(2, '0')).join('');
}
function mix(a: string, b: string, t: number): string {
  const A = rgb(a), B = rgb(b);
  return hex(A.map((v, i) => v + (B[i] - v) * t));
}
// WCAG relative luminance, as HexColor::relative_luminance computes it.
function luminance(h: string): number {
  const ch = (c: number) => { const s = c / 255; return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4; };
  const [r, g, b] = rgb(h);
  return 0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b);
}
const blendOf = (t: Theme) => t.heat.blend ?? (luminance(t.bg) > 0.18 ? 'multiply' : 'screen');

function attr(el: Element | null, values: Record<string, string | number>) {
  if (!el) return;
  for (const [k, v] of Object.entries(values)) el.setAttribute(k, String(v));
}

function mount(host: HTMLElement, text: string): SVGSVGElement {
  const doc = new DOMParser().parseFromString(text, 'image/svg+xml');
  const svg = document.importNode(doc.documentElement, true) as unknown as SVGSVGElement;
  if (svg.nodeName !== 'svg') throw new Error('the plate is not an SVG');
  svg.removeAttribute('width');
  svg.removeAttribute('height');
  const paper = document.createElementNS(SVG_NS, 'rect');
  attr(paper, { class: 'paper', x: 0, y: 0, width: 1200, height: 1600 });
  svg.insertBefore(paper, svg.firstChild);
  // The text plate, drawn page-side: --web renders none. City, count and
  // a 5 km bar (24 px/km at this radius); never coordinates.
  const typo = document.createElementNS(SVG_NS, 'g');
  attr(typo, { class: 'typography', 'font-family': "'IBM Plex Sans', system-ui, sans-serif" });
  typo.innerHTML =
    '<text x="48" y="1520" font-size="64" letter-spacing="18" font-weight="300">GRAND RAPIDS</text>' +
    '<text class="count" x="1152" y="1518" text-anchor="end" font-size="13" letter-spacing="3.5" font-weight="500" opacity=".7"></text>' +
    '<g opacity=".8"><path d="M48 1420H168M48 1413V1427M168 1413V1427" fill="none" stroke-width="1.5"/>' +
    '<text x="48" y="1405" font-size="11" letter-spacing="2">0</text><text x="168" y="1405" text-anchor="end" font-size="11" letter-spacing="2">5 KM</text></g>';
  svg.appendChild(typo);
  host.replaceChildren(svg);
  return svg;
}

function apply(svg: SVGSVGElement, s: State) {
  const t = s.theme, h = t.heat;
  const q = (sel: string) => svg.querySelector(sel);
  attr(q('.paper'), { fill: s.paper ? t.bg : 'none' });
  attr(q('.water'), { fill: t.water });
  attr(q('.water-lines'), { stroke: t.water, 'stroke-width': t.water_line_width ?? 1 });
  attr(q('.parks'), { fill: t.parks });
  for (const tier of ['motorway', 'trunk', 'primary', 'secondary'] as const) {
    const r = t[`road_${tier}`];
    attr(q('.' + tier), { stroke: r.color, 'stroke-width': r.width });
  }
  for (const sel of ['.water', '.water-lines', '.parks', '.roads']) {
    const g = q(sel) as SVGElement | null;
    if (g) g.style.display = s.basemap ? '' : 'none';
  }

  attr(q('.heat-stack'), { stroke: h.color });
  const bloom = s.bloom;
  const halo = (a: number) => Math.min(1, a * bloom * (s.web ? WEB_GLOW : 1));
  const outer = q('.heat-outer') as SVGElement, inner = q('.heat-inner') as SVGElement, core = q('.heat-core') as SVGElement;
  attr(outer, { 'stroke-width': h.width * (h.glow_outer_ratio ?? GLOW.outer_ratio) * bloom, 'stroke-opacity': halo(h.glow_outer_alpha ?? GLOW.outer_alpha) });
  attr(inner, { 'stroke-width': h.width * (h.glow_inner_ratio ?? GLOW.inner_ratio) * bloom, 'stroke-opacity': halo(h.glow_inner_alpha ?? GLOW.inner_alpha) });
  attr(core, { 'stroke-width': h.width, 'stroke-opacity': Math.min(1, h.alpha * s.alpha) });
  outer.style.display = inner.style.display = bloom > 0 ? '' : 'none';
  core.style.mixBlendMode = blendOf(t);

  let shown = 0;
  svg.querySelectorAll<SVGPathElement>('.heat path').forEach((p) => {
    const on = s.types.size === 0 || s.types.has(p.getAttribute('data-type') ?? '');
    p.style.display = on ? '' : 'none';
    if (on) shown++;
  });

  const typo = q('.typography') as SVGGElement;
  attr(typo, { fill: t.text });
  attr(typo.querySelector('path'), { stroke: t.text });
  typo.querySelector('.count')!.textContent = shown + ' ACTIVITIES';
  typo.style.display = !s.web && s.crop === 'poster' ? '' : 'none';
  svg.setAttribute('viewBox', CROPS[s.crop].vb.join(' '));
  return shown;
}

/** The command the proof corresponds to. Real flags from src/cli.rs only. */
export function command(s: State, name: string, needsDir: boolean): string {
  const parts = ['patinate render'];
  if (needsDir) parts.push('--themes-dir ./themes');
  parts.push('--theme ' + name);
  if (s.types.size) {
    const t = [...s.types];
    parts.push(t.length === 2 && s.types.has('Ride') && s.types.has('EBikeRide') ? '--cycling' : '--type ' + t.join(','));
  }
  if (s.web) parts.push('--web');
  if (!s.basemap) parts.push('--heat-only');
  if (!s.paper) parts.push('--transparent-bg');
  if (s.bloom !== 1) parts.push('--heat-bloom ' + s.bloom);
  if (s.alpha !== 1) parts.push('--heat-alpha ' + s.alpha);
  parts.push(...CROPS[s.crop].flags);
  if (s.anonymize) parts.push('--anonymize');
  parts.push('--out ' + name + '.svg');
  return parts.join(' \\\n  ');
}

function rasterize(svg: SVGSVGElement, w: number, h: number): Promise<HTMLCanvasElement> {
  return new Promise((resolve, reject) => {
    const c = svg.cloneNode(true) as SVGSVGElement;
    attr(c, { xmlns: SVG_NS, width: w, height: h });
    const url = URL.createObjectURL(new Blob([new XMLSerializer().serializeToString(c)], { type: 'image/svg+xml;charset=utf-8' }));
    const img = new Image();
    img.onload = () => {
      const canvas = document.createElement('canvas');
      canvas.width = w; canvas.height = h;
      canvas.getContext('2d')!.drawImage(img, 0, 0, w, h);
      URL.revokeObjectURL(url);
      resolve(canvas);
    };
    img.onerror = () => { URL.revokeObjectURL(url); reject(new Error('the browser could not rasterize the proof')); };
    img.src = url;
  });
}

function download(name: string, blob: Blob) {
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = name;
  document.body.appendChild(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 2000);
}

export async function startPress() {
  const themes = JSON.parse($('#press-themes').textContent ?? '[]') as Theme[];
  const shipped = new Set(themes.map((t) => t.name));
  const byName = (n: string) => themes.find((t) => t.name === n) ?? themes[0];
  const fromHash = () => decodeURIComponent(location.hash.slice(1));

  const first = byName(fromHash());
  const s: State = {
    name: first.name, theme: clone(first), edited: false, web: false, bloom: 1, alpha: 1,
    basemap: true, paper: true, anonymize: true, types: new Set(), crop: 'poster',
  };
  const outName = () => (s.edited || !shipped.has(s.name) ? s.name + '_proof' : s.name);
  const needsDir = () => s.edited || !shipped.has(s.name);

  const host = $('#proof');
  let svg: SVGSVGElement;
  try {
    const res = await fetch(PLATE);
    if (!res.ok) throw new Error(PLATE + ': ' + res.status);
    svg = mount(host, await res.text());
  } catch (e) {
    host.innerHTML = '<div class="loading">plate unavailable</div>';
    console.error(e);
    return;
  }

  const counts: Record<string, number> = {};
  svg.querySelectorAll('.heat path').forEach((p) => {
    const k = p.getAttribute('data-type') ?? '';
    counts[k] = (counts[k] ?? 0) + 1;
  });
  const total = Object.values(counts).reduce((a, b) => a + b, 0);

  // Controls
  const sel = $<HTMLSelectElement>('#edition');
  for (const t of themes) sel.add(new Option(t.name, t.name));
  const typesEl = $('#types');
  const chip = (label: string, key: string, n: number) => {
    const b = document.createElement('button');
    b.type = 'button'; b.className = 'chip'; b.dataset.type = key;
    b.innerHTML = label + '<span class="n">' + n + '</span>';
    b.addEventListener('click', () => {
      if (key === 'all') s.types.clear();
      else if (s.types.has(key)) s.types.delete(key);
      else s.types.add(key);
      if (s.types.size === TYPES.length) s.types.clear();
      render();
    });
    typesEl.appendChild(b);
  };
  typesEl.replaceChildren();
  chip('all', 'all', total);
  for (const k of TYPES) chip(k, k, counts[k] ?? 0);

  const cropsEl = $('#crops');
  for (const [k, c] of Object.entries(CROPS) as [Crop, (typeof CROPS)[Crop]][]) {
    const b = document.createElement('button');
    b.type = 'button'; b.className = 'chip'; b.dataset.crop = k; b.textContent = c.label;
    b.addEventListener('click', () => { s.crop = k; render(); });
    cropsEl.appendChild(b);
  }

  document.querySelectorAll<HTMLButtonElement>('#toggles .chip').forEach((b) => {
    b.addEventListener('click', () => {
      const k = b.dataset.toggle as 'web' | 'basemap' | 'paper' | 'anonymize';
      s[k] = !s[k];
      render();
    });
  });

  const range = (id: string, out: string, set: (v: number) => void, fmt: (v: number) => string) => {
    const el = $<HTMLInputElement>(id);
    el.addEventListener('input', () => { set(parseFloat(el.value)); $(out).textContent = fmt(parseFloat(el.value)); render(); });
  };
  range('#heat-width', '#o-width', (v) => { s.theme.heat.width = v; s.edited = true; }, (v) => v.toFixed(2));
  range('#heat-alpha', '#o-alpha', (v) => { s.alpha = v; }, (v) => v.toFixed(1));
  range('#heat-bloom', '#o-bloom', (v) => { s.bloom = v; }, (v) => v.toFixed(1));

  document.querySelectorAll<HTMLInputElement>('input[type="color"][data-ink]').forEach((inp) => {
    inp.addEventListener('input', () => {
      const t = s.theme, v = inp.value;
      switch (inp.dataset.ink) {
        case 'bg': t.bg = v; t.fade_top.from = v; t.fade_bottom.from = v; break;
        case 'roads': {
          // One road ink sets motorway; the lower tiers mix toward the ground.
          const steps = [0, 0.15, 0.35, 0.55, 0.72, 0.85];
          const tiers = ['motorway', 'trunk', 'primary', 'secondary', 'tertiary', 'residential'] as const;
          tiers.forEach((tier, i) => { t[`road_${tier}`].color = mix(v, t.bg, steps[i]); });
          break;
        }
        case 'heat': t.heat.color = v; break;
        case 'water': t.water = v; break;
        case 'parks': t.parks = v; break;
        case 'text': t.text = v; break;
      }
      s.edited = true;
      render();
    });
  });

  function syncControls() {
    const t = s.theme;
    sel.value = s.name;
    const inks: [string, string][] = [['paper', t.bg], ['roads', t.road_motorway.color], ['water', t.water], ['parks', t.parks], ['heat', t.heat.color], ['text', t.text]];
    for (const [k, v] of inks) { $<HTMLInputElement>('#ink-' + k).value = v; $('#v-' + k).textContent = v; }
    $<HTMLInputElement>('#heat-width').value = String(t.heat.width); $('#o-width').textContent = t.heat.width.toFixed(2);
    $<HTMLInputElement>('#heat-alpha').value = String(s.alpha); $('#o-alpha').textContent = s.alpha.toFixed(1);
    $<HTMLInputElement>('#heat-bloom').value = String(s.bloom); $('#o-bloom').textContent = s.bloom.toFixed(1);
  }

  function render() {
    const t = s.theme;
    const shown = apply(svg, s);
    const vb = CROPS[s.crop].vb;
    host.style.aspectRatio = vb[2] + ' / ' + vb[3];
    host.style.setProperty('--proof-paper', s.paper ? t.bg : 'transparent');
    const name = outName();
    $('#proof-label').textContent = (s.edited ? 'proof · ' : 'theme · ') + name;
    $('#proof-count').textContent = shown + ' of ' + total + ' activities';
    $('#proof-inkbar').replaceChildren(...[t.bg, t.road_motorway.color, t.road_primary.color, t.water, t.parks, t.heat.color, t.text].map((c) => {
      const i = document.createElement('i'); i.style.background = c; return i;
    }));
    $('#cmd code').textContent = command(s, name, needsDir());
    $('#v-paper').textContent = t.bg;
    for (const [k, v] of [['roads', t.road_motorway.color], ['water', t.water], ['parks', t.parks], ['heat', t.heat.color], ['text', t.text]] as const) {
      $('#v-' + k).textContent = v;
    }
    typesEl.querySelectorAll<HTMLElement>('.chip').forEach((b) => {
      const k = b.dataset.type ?? '';
      b.setAttribute('aria-pressed', String(k === 'all' ? s.types.size === 0 : s.types.has(k)));
    });
    cropsEl.querySelectorAll<HTMLElement>('.chip').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.crop === s.crop)));
    document.querySelectorAll<HTMLElement>('#toggles .chip').forEach((b) => {
      b.setAttribute('aria-pressed', String(s[b.dataset.toggle as 'web' | 'basemap' | 'paper' | 'anonymize']));
    });
  }

  function load(name: string) {
    const t = byName(name);
    s.name = t.name; s.theme = clone(t); s.edited = false;
    syncControls();
    render();
    if (fromHash() !== t.name) history.replaceState(null, '', '#' + t.name);
  }

  sel.addEventListener('change', () => load(sel.value));
  window.addEventListener('hashchange', () => { if (fromHash() !== s.name) load(fromHash()); });
  $('#reset').addEventListener('click', () => {
    Object.assign(s, { web: false, bloom: 1, alpha: 1, basemap: true, paper: true, anonymize: true, crop: 'poster' });
    s.types.clear();
    load(s.name);
  });
  $('#save-json').addEventListener('click', () => {
    const t = clone(s.theme);
    t.name = outName();
    download(t.name + '.json', new Blob([JSON.stringify(t, null, 2) + '\n'], { type: 'application/json' }));
    $('#pull-status').textContent = needsDir()
      ? 'Saved ' + t.name + '.json. Put it in ./themes; the command above already passes --themes-dir.'
      : 'Saved ' + t.name + '.json, the shipped theme as the binary embeds it.';
  });
  const pull = $<HTMLButtonElement>('#pull-png');
  pull.addEventListener('click', async () => {
    pull.disabled = true;
    $('#pull-status').textContent = 'Rasterizing…';
    try {
      const vb = CROPS[s.crop].vb;
      const w = 2400, h = Math.round((2400 * vb[3]) / vb[2]);
      const canvas = await rasterize(svg, w, h);
      canvas.toBlob((blob) => {
        if (blob) download('patinate-' + outName() + '.png', blob);
        $('#pull-status').textContent = 'Saved a ' + w + ' × ' + h + ' PNG of the proof.';
        pull.disabled = false;
      }, 'image/png');
    } catch (e) {
      $('#pull-status').textContent = e instanceof Error ? e.message : 'The pull failed.';
      pull.disabled = false;
    }
  });

  load(s.name);
}

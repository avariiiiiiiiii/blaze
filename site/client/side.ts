/**
 * The sidebar on every page (renderer core side.rs, styles/shell.css). Everything here is an enhancement: without it
 * sections still fold (`<details>`), the phone drawer still opens (`#menu` and `:target`) and the outline is links.
 *
 * - Folds: a section the reader folded stays folded on the next page (localStorage), except the one holding the page.
 * - The drawer: Menu opens it, the scrim, the close button and Escape close it, without touching the URL.
 * - Search: the pill (and ⌘K, Ctrl K or /) opens a dialog over `search.json` (gen/build.ts writes it: one entry per
 *   page and per section, with its text); every word typed must appear, title and heading matches rank first.
 * - The outline: the section being read is marked, as the ix docs' "On this page" does.
 */

type Entry = { u: string; t: string; h: string; x: string };

const FOLDED = 'blaze.side.folded';

function readFolded(): Set<string> {
	try {
		const raw = localStorage.getItem(FOLDED);
		const v: unknown = raw === null ? [] : JSON.parse(raw);
		return new Set(Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : []);
	} catch {
		return new Set();
	}
}

function writeFolded(s: Set<string>) {
	try {
		localStorage.setItem(FOLDED, JSON.stringify([...s]));
	} catch {
		// Private mode or storage off: folds just don't carry over.
	}
}

function folds() {
	const folded = readFolded();
	for (const sec of document.querySelectorAll<HTMLDetailsElement>('details.sec[data-sec]')) {
		const key = sec.dataset.sec!;
		if (folded.has(key) && !('here' in sec.dataset)) sec.open = false;
		sec.addEventListener('toggle', () => {
			const now = readFolded();
			if (sec.open) now.delete(key);
			else now.add(key);
			writeFolded(now);
		});
	}
	// The open page's row in view inside the sidebar's own scroller.
	const row = document.querySelector<HTMLElement>('.navlist [aria-current="page"]');
	const box = document.querySelector<HTMLElement>('.scrollbox');
	if (row && box) box.scrollTop += row.getBoundingClientRect().top - box.getBoundingClientRect().top - box.clientHeight / 2 + row.offsetHeight / 2;
}

function drawer() {
	const side = document.getElementById('menu');
	if (!side) return;
	const open = (e: Event) => {
		e.preventDefault();
		side.classList.add('open');
		side.querySelector<HTMLElement>('[aria-current="page"], a')?.focus({ preventScroll: true });
	};
	const close = (e?: Event) => {
		e?.preventDefault();
		side.classList.remove('open');
		// Opened without JS first (`#menu` in the URL): drop the target too.
		if (location.hash === '#menu') history.replaceState(history.state, '', location.pathname + location.search);
	};
	for (const b of document.querySelectorAll('a.menubtn[href="#menu"]')) b.addEventListener('click', open);
	for (const b of document.querySelectorAll('.scrim, .closebtn')) b.addEventListener('click', close);
	document.addEventListener('keydown', (e) => {
		if (e.key === 'Escape' && side.classList.contains('open')) close();
	});
}

/** The site's base (`/blaze/`), from the stylesheet the renderer linked as `<base>/site.css`. */
function base(): string {
	const css = document.querySelector<HTMLLinkElement>('link[rel="stylesheet"][href$="site.css"]');
	return css ? new URL('.', css.href).pathname : '/';
}

const words = (q: string) => q.toLowerCase().split(/\s+/).filter(Boolean);

function rank(entries: Entry[], q: string): Entry[] {
	const ws = words(q);
	if (ws.length === 0) return [];
	const scored: [number, Entry][] = [];
	for (const e of entries) {
		const t = e.t.toLowerCase();
		const h = e.h.toLowerCase();
		const x = e.x.toLowerCase();
		let score = 0;
		let all = true;
		for (const w of ws) {
			if (t.includes(w)) score += h === '' ? 12 : 4;
			else if (h.includes(w)) score += 8;
			else if (x.includes(w)) score += 1;
			else {
				all = false;
				break;
			}
		}
		if (all) scored.push([score, e]);
	}
	return scored.sort((a, b) => b[0] - a[0]).slice(0, 20).map(([, e]) => e);
}

function snippet(x: string, q: string): string {
	const w = words(q)[0] ?? '';
	const at = Math.max(0, x.toLowerCase().indexOf(w) - 40);
	return (at > 0 ? '…' : '') + x.slice(at, at + 160);
}

function search() {
	const buttons = [...document.querySelectorAll<HTMLButtonElement>('[data-search]')];
	if (buttons.length === 0 || typeof HTMLDialogElement === 'undefined') return;
	const mac = /Mac|iPhone|iPad/.test(navigator.platform);
	for (const b of buttons) {
		b.hidden = false;
		const kbd = b.querySelector('kbd');
		if (kbd && !mac) kbd.textContent = 'Ctrl K';
	}

	const dialog = document.createElement('dialog');
	dialog.className = 'search';
	dialog.setAttribute('aria-label', 'Search the site');
	const icon = buttons[0]!.querySelector('svg')?.outerHTML ?? '';
	dialog.innerHTML = `<form method="dialog" role="search">${icon}<input type="search" placeholder="Search blaze" aria-label="Search" autocomplete="off" spellcheck="false" /></form><ul role="listbox"></ul>`;
	document.body.append(dialog);
	const input = dialog.querySelector('input')!;
	const list = dialog.querySelector('ul')!;
	let entries: Entry[] | null = null;
	let shown: HTMLAnchorElement[] = [];
	let sel = 0;

	const select = (i: number) => {
		shown.forEach((a, j) => a.setAttribute('aria-selected', String(j === i)));
		sel = i;
		shown[i]?.scrollIntoView({ block: 'nearest' });
	};
	const draw = () => {
		const q = input.value.trim();
		list.replaceChildren();
		shown = [];
		if (entries === null || q === '') return;
		const hits = rank(entries, q);
		if (hits.length === 0) {
			const li = document.createElement('li');
			li.className = 'none';
			li.textContent = 'Nothing matches.';
			list.append(li);
			return;
		}
		for (const e of hits) {
			const li = document.createElement('li');
			const a = document.createElement('a');
			a.href = e.u;
			a.setAttribute('role', 'option');
			const t = document.createElement('span');
			t.className = 't';
			t.textContent = e.h ? `${e.t} › ${e.h}` : e.t;
			const p = document.createElement('span');
			p.className = 'p';
			p.textContent = snippet(e.x, q);
			a.append(t, p);
			a.addEventListener('click', () => dialog.close());
			li.append(a);
			list.append(li);
			shown.push(a);
		}
		select(0);
	};
	const load = async () => {
		if (entries !== null) return;
		try {
			const res = await fetch(`${base()}search.json`);
			const v: unknown = await res.json();
			entries = Array.isArray(v) ? (v as Entry[]) : [];
		} catch {
			entries = [];
		}
		draw();
	};
	const open = () => {
		document.getElementById('menu')?.classList.remove('open');
		dialog.showModal();
		input.select();
		void load();
	};

	input.addEventListener('input', draw);
	input.addEventListener('keydown', (e) => {
		if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
			e.preventDefault();
			if (shown.length) select((sel + (e.key === 'ArrowDown' ? 1 : shown.length - 1)) % shown.length);
		} else if (e.key === 'Enter') {
			e.preventDefault();
			const a = shown[sel];
			if (a) {
				dialog.close();
				location.href = a.href;
			}
		}
	});
	dialog.addEventListener('click', (e) => {
		if (e.target === dialog) dialog.close();
	});
	for (const b of buttons) b.addEventListener('click', open);
	document.addEventListener('keydown', (e) => {
		const typing = e.target instanceof HTMLElement && e.target.closest('input, textarea, [contenteditable]');
		if ((e.key === 'k' && (e.metaKey || e.ctrlKey)) || (e.key === '/' && !typing)) {
			e.preventDefault();
			if (dialog.open) dialog.close();
			else open();
		}
	});
}

function spy() {
	const links = [...document.querySelectorAll<HTMLAnchorElement>('.toc a[href^="#"]')];
	if (links.length === 0) return;
	const heads = links.map((a) => document.getElementById(decodeURIComponent(a.hash.slice(1))));
	let frame = 0;
	const mark = () => {
		frame = 0;
		let cur = -1;
		heads.forEach((h, i) => {
			if (h && h.getBoundingClientRect().top <= 96) cur = i;
		});
		links.forEach((a, i) => a.classList.toggle('cur', i === cur));
	};
	addEventListener('scroll', () => (frame ||= requestAnimationFrame(mark)), { passive: true });
	mark();
}

folds();
drawer();
search();
spy();

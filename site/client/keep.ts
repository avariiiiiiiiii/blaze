/**
 * The sidebar as the last page left it. A classic script placed right after the sidebar's markup (render.rs), so it
 * runs while the page is still parsing, before the first paint: the folded sections close and the sidebar's scroller
 * goes back to where it was, and moving to another page doesn't make the sidebar jump. side.ts writes both (the
 * folds on toggle, the scroll on pagehide) and marks the scroller `data-kept` so it doesn't re-centre the open row.
 * A block, not a module: it must not wait for the parse to end.
 */
{
	try {
		const raw = localStorage.getItem('blaze.side.folded');
		const folded: unknown = raw === null ? [] : JSON.parse(raw);
		if (Array.isArray(folded)) {
			for (const sec of document.querySelectorAll<HTMLDetailsElement>('details.sec[data-sec]')) {
				if (folded.includes(sec.dataset.sec) && !('here' in sec.dataset)) sec.open = false;
			}
		}
		const box = document.querySelector<HTMLElement>('.side .scrollbox');
		const top = sessionStorage.getItem('blaze.side.scroll');
		if (box && top !== null) {
			box.scrollTop = Number(top);
			box.dataset.kept = '';
		}
	} catch {
		// Storage off: the sidebar opens as the page renders it.
	}
}

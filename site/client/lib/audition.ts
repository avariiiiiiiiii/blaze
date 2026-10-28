/**
 * /learn/sounds (content/learn/sounds.flecs, the SoundBoard widget): every lesson sound in every palette, to pick by
 * ear. A card per palette with a button per sound and "Use in lessons", which keeps the pick (sound.ts,
 * `blaze.sound.palette`). Each button shows the sound's peak level, rendered offline through the same chain, so the
 * loudness can be checked without listening.
 *
 * Under client/lib/ (not bundled as an entry), imported by client/learn.ts.
 */

import { audition, auditionStreak, enabled, palette, palettes, peak, setEnabled, setPalette, type SoundName } from './sound';

const SOUNDS: { name: SoundName | 'streak'; label: string }[] = [
	{ name: 'right', label: 'Right' },
	{ name: 'streak', label: 'Five right in a row' },
	{ name: 'wrong', label: 'Not quite' },
	{ name: 'tick', label: 'Continue' },
	{ name: 'pick', label: 'Choose' },
	{ name: 'done', label: 'Lesson complete' },
	{ name: 'milestone', label: 'Every question right' },
];

export function soundBoard(board: HTMLElement, signal: AbortSignal): () => void {
	board.replaceChildren();
	const muted = document.createElement('p');
	muted.className = 'sb-muted';
	const paintMuted = () => {
		muted.hidden = enabled();
		muted.innerHTML = 'Sounds are off in the lessons. These buttons play anyway. <button type="button">Turn sounds on</button>';
		muted.querySelector('button')?.addEventListener('click', () => {
			setEnabled(true);
			paintMuted();
		});
	};
	paintMuted();
	board.append(muted);

	const cards = palettes.map((pal) => {
		const card = document.createElement('section');
		card.className = 'sb-card';
		card.dataset.palette = pal.id;
		card.innerHTML = `<header><h3></h3><p></p></header><div class="sb-sounds"></div><button type="button" class="sb-use"></button>`;
		card.querySelector('h3')!.textContent = pal.name;
		card.querySelector('header p')!.textContent = pal.about;
		const list = card.querySelector('.sb-sounds')!;
		for (const s of SOUNDS) {
			const b = document.createElement('button');
			b.type = 'button';
			b.className = 'sb-play';
			b.dataset.sound = s.name;
			b.innerHTML = `<span class="sb-l"></span><span class="sb-db" aria-hidden="true"></span>`;
			b.querySelector('.sb-l')!.textContent = s.label;
			b.addEventListener('click', () => {
				if (s.name === 'streak') auditionStreak(pal);
				else audition(s.name, pal);
				b.classList.remove('hit');
				void b.offsetWidth;
				b.classList.add('hit');
			});
			list.append(b);
		}
		card.querySelector('.sb-use')!.addEventListener('click', () => {
			setPalette(pal.id);
			paintUse();
			audition('right', pal);
		});
		board.append(card);
		return card;
	});

	function paintUse() {
		const cur = palette().id;
		for (const c of cards) {
			const mine = c.dataset.palette === cur;
			c.toggleAttribute('data-active', mine);
			const u = c.querySelector<HTMLButtonElement>('.sb-use')!;
			u.textContent = mine ? 'Used in the lessons' : 'Use in the lessons';
			u.setAttribute('aria-pressed', String(mine));
		}
	}
	paintUse();

	// Peak levels, rendered offline one at a time once the page is idle.
	let stopped = false;
	void (async () => {
		await new Promise((r) => setTimeout(r, 300));
		for (const c of cards) {
			const pal = palettes.find((p) => p.id === c.dataset.palette)!;
			for (const b of c.querySelectorAll<HTMLButtonElement>('.sb-play')) {
				if (stopped || signal.aborted) return;
				const name = b.dataset.sound as SoundName | 'streak';
				try {
					// A streak's loudest note is its last, highest figure.
					const db = name === 'streak' ? await peak('right', pal, 4) : await peak(name, pal);
					b.dataset.peak = db.toFixed(1);
					b.querySelector('.sb-db')!.textContent = `${Math.round(db)} dB`;
				} catch {
					return; // No OfflineAudioContext: no levels.
				}
			}
		}
		board.dataset.measured = '';
	})();
	return () => {
		stopped = true;
	};
}

/**
 * The reader's progress through the course, kept in localStorage under `blaze.learn` (wrapped: storage can be missing,
 * full or blocked, and then progress lasts for the visit). client/learn.ts writes it as the reader goes; the course
 * map (learn.ts) and the sidebar's lesson marks (side.ts) read it.
 *
 * Under client/lib/ (not bundled as an entry).
 */

export type Pass = { tries: number; right: boolean };
export type Progress = { step: number; seen: number; done: boolean; pass: Record<string, Pass> };
export type Saved = { v: 1; lessons: Record<string, Progress> };
/** todo: not opened; doing: started; done: finished; mastered: finished, every question right the first time. */
export type State = 'todo' | 'doing' | 'done' | 'mastered';

export const KEY = 'blaze.learn';
let memory: Saved = { v: 1, lessons: {} };

export function load(): Saved {
	try {
		const raw = localStorage.getItem(KEY);
		if (raw) {
			const s: unknown = JSON.parse(raw);
			if (s && typeof s === 'object' && 'v' in s && s.v === 1 && 'lessons' in s && s.lessons && typeof s.lessons === 'object')
				memory = s as Saved;
		}
	} catch {
		// No storage, or not ours: keep what this visit has.
	}
	return memory;
}

export function store(s: Saved) {
	memory = s;
	try {
		localStorage.setItem(KEY, JSON.stringify(s));
	} catch {
		// Full or blocked: the progress lasts for this visit.
	}
}

/** A lesson's progress, made empty if it has none yet. */
export function progress(s: Saved, lesson: string): Progress {
	const p = s.lessons[lesson];
	if (p && typeof p.step === 'number') {
		p.pass ??= {};
		p.seen ??= p.step;
		return p;
	}
	return (s.lessons[lesson] = { step: 0, seen: 0, done: false, pass: {} });
}

/** How many of `quizzes` were right the first time. */
export function firstTime(p: Progress | undefined, quizzes: string[]): number {
	return quizzes.filter((id) => p?.pass?.[id]?.right && p.pass[id].tries === 1).length;
}

export function stateOf(p: Progress | undefined, quizzes: string[]): State {
	if (p?.done) return firstTime(p, quizzes) === quizzes.length ? 'mastered' : 'done';
	return p && p.seen > 0 ? 'doing' : 'todo';
}

export const stateWords: Record<State, string> = { todo: 'not started', doing: 'in progress', done: 'finished', mastered: 'mastered' };

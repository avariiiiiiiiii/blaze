/**
 * The lessons' sounds (client/learn.ts), synthesised with Web Audio, so there are no files to load: a soft two-note
 * chime for a right answer, a low muted tone for a miss (never a buzzer), a very quiet tick for Continue and for a
 * pick, and a short resolving phrase when a lesson is finished.
 *
 * - One AudioContext, made on the reader's first tap or key and resumed inside that handler (iOS Safari starts it
 *   only from a gesture). Every sound is played from a click or key handler, so nothing sounds before the reader has
 *   done something.
 * - Quiet: everything goes through one gain at a low level and a gentle low-pass.
 * - On by default; the lesson's top bar has the switch, kept in localStorage (`blaze.sound`), wrapped as all storage
 *   is, since it can be missing.
 *
 * Under client/lib/ (not bundled as an entry), imported by learn.ts.
 */

const KEY = 'blaze.sound';
let on = true;
try {
	on = localStorage.getItem(KEY) !== 'off';
} catch {
	// No storage: on, for this visit.
}

let ctx: AudioContext | null = null;
let out: GainNode | null = null;

export const enabled = () => on;

export function setEnabled(v: boolean) {
	on = v;
	try {
		localStorage.setItem(KEY, v ? 'on' : 'off');
	} catch {
		// Kept for this visit only.
	}
	if (v) unlock();
	else void ctx?.suspend().catch(() => {});
}

type AudioCtor = typeof AudioContext;

/** Makes or resumes the context. Call it from inside a gesture's handler. */
function unlock(): AudioContext | null {
	if (!on) return null;
	try {
		if (!ctx) {
			const AC: AudioCtor | undefined = window.AudioContext ?? (window as unknown as { webkitAudioContext?: AudioCtor }).webkitAudioContext;
			if (!AC) return null;
			ctx = new AC();
			const lp = ctx.createBiquadFilter();
			lp.type = 'lowpass';
			lp.frequency.value = 4200;
			out = ctx.createGain();
			out.gain.value = 0.16;
			out.connect(lp).connect(ctx.destination);
		}
		if (ctx.state !== 'running') void ctx.resume().catch(() => {});
		return ctx;
	} catch {
		return null;
	}
}

type Note = {
	/** Seconds from now. */
	at: number;
	freq: number;
	/** Seconds until it has faded out. */
	dur: number;
	gain: number;
	type?: OscillatorType;
	/** Low-pass cutoff for this note, for the muted ones. */
	cut?: number;
	/** Glide to this frequency over the note. */
	to?: number;
	attack?: number;
};

function play(notes: Note[]) {
	const c = unlock();
	if (!c || !out) return;
	const t0 = c.currentTime + 0.01;
	for (const n of notes) {
		const t = t0 + n.at;
		const osc = c.createOscillator();
		osc.type = n.type ?? 'sine';
		osc.frequency.setValueAtTime(n.freq, t);
		if (n.to) osc.frequency.exponentialRampToValueAtTime(n.to, t + n.dur);
		const env = c.createGain();
		env.gain.setValueAtTime(0, t);
		env.gain.linearRampToValueAtTime(n.gain, t + (n.attack ?? 0.006));
		env.gain.exponentialRampToValueAtTime(0.0001, t + n.dur);
		let node: AudioNode = osc.connect(env);
		if (n.cut) {
			const f = c.createBiquadFilter();
			f.type = 'lowpass';
			f.frequency.value = n.cut;
			node = env.connect(f);
		}
		node.connect(out);
		osc.start(t);
		osc.stop(t + n.dur + 0.05);
	}
}

/** A bell-ish note: the tone and a faint octave above it. */
const bell = (at: number, freq: number, dur: number, gain: number): Note[] => [
	{ at, freq, dur, gain },
	{ at, freq: freq * 2, dur: dur * 0.6, gain: gain * 0.18, type: 'triangle' },
];

export const sound = {
	/** Continue: barely there. */
	tick: () => play([{ at: 0, freq: 1320, dur: 0.035, gain: 0.22, type: 'triangle', cut: 2600, attack: 0.002 }]),
	/** Picking an answer or a part. */
	pick: () => play([{ at: 0, freq: 880, dur: 0.05, gain: 0.26, cut: 2200, attack: 0.002 }]),
	/** Right: G5 then D6, rising. */
	right: () => play([...bell(0, 783.99, 0.42, 0.55), ...bell(0.085, 1174.66, 0.6, 0.5)]),
	/** Not quite: one low, short, muffled tone that sinks a little. */
	wrong: () => play([{ at: 0, freq: 220, to: 174.61, dur: 0.26, gain: 0.7, type: 'triangle', cut: 700, attack: 0.01 }]),
	/** Lesson finished: C5 E5 G5, landing on C6. */
	done: () =>
		play([
			...bell(0, 523.25, 0.5, 0.42),
			...bell(0.11, 659.25, 0.5, 0.42),
			...bell(0.22, 783.99, 0.55, 0.42),
			...bell(0.36, 1046.5, 1.1, 0.48),
		]),
};

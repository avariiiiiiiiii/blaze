/**
 * The /updates signup form (SignupForm in renderer/core/src/render.rs), sent with fetch so the page stays put. Set up
 * again on every page (client/lib/page.ts). With Turnstile on, /updates is always a full load (client/nav.ts: its
 * script is from another origin), so `window.turnstile` is this page's own.
 */
import { onPage } from './lib/page';

declare global {
	interface Window {
		turnstile?: { reset(widget?: string): void };
	}
}

function init(signal: AbortSignal) {
	const form = document.querySelector<HTMLFormElement>('form.signup');
	if (!form) return;
	const button = form.querySelector<HTMLButtonElement>('button[type=submit]');
	const error = form.querySelector<HTMLParagraphElement>('p.error');
	const sent = document.querySelector<HTMLDivElement>('div.sent');
	const label = button?.textContent ?? 'Send';

	const fail = (message: string) => {
		if (!error) return alert(message);
		error.textContent = message;
		error.hidden = false;
	};

	form.addEventListener('submit', async (e) => {
		e.preventDefault();
		if (button?.disabled) return;
		const data = new FormData(form);
		if (button) {
			button.disabled = true;
			button.textContent = 'Sending…';
		}
		if (error) error.hidden = true;
		try {
			// location.href keeps ?src / utm_* for the worker.
			const res = await fetch(location.href, { method: 'POST', body: data, headers: { Accept: 'application/json' } });
			let reply: { ok?: boolean; error?: string } = {};
			try {
				reply = (await res.json()) as typeof reply;
			} catch {
				// not JSON
			}
			if (res.ok && reply.ok) {
				form.hidden = true;
				form.style.display = 'none'; // form.signup's display: flex beats [hidden]
				if (sent) sent.hidden = false;
			} else fail(reply.error ?? `Something went wrong (${res.status}). Try again.`);
		} catch {
			fail('Could not reach the server. Check your connection and try again.');
		} finally {
			if (button) {
				button.disabled = false;
				button.textContent = label;
			}
			window.turnstile?.reset();
		}
	}, { signal });
}

onPage(init);

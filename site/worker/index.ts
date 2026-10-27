/**
 * The blaze site worker, at https://ix.dev/blaze/ (worker/base.ts). Every request comes here first
 * (wrangler.jsonc `run_worker_first: true`): an old host or the bare base is redirected, the base is taken off the
 * path, and then, by site path:
 *
 *   /auth/*    GitHub sign-in and sign-out (auth.ts)
 *   /api/*     comments (comments.ts)
 *   /updates   the signup form (updates.ts)
 *   /charter, /rfcs/*   discussable pages, rendered with their comments (pages.ts); other paths there are assets
 *   /audio/*   narration files, from R2 when bound, with Range support (audio.ts)
 *
 * Anything else is dist/ (built by `bun gen/build.ts`) through the ASSETS binding, with the headers `assetHeaders`
 * adds (Workers does not apply a `_headers` file to what a worker returns).
 */
import { handleAudio } from './audio';
import { handleAuth } from './auth';
import { enter } from './base';
import { handleComments } from './comments';
import type { Env } from './env';
import { assetHeaders, json, plain } from './http';
import { handlePage } from './pages';
import { handleUpdates } from './updates';

async function route(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
	const url = new URL(request.url);
	const p = url.pathname;
	if (p === '/updates') return handleUpdates(request, env, url);
	if (p === '/auth' || p.startsWith('/auth/')) return handleAuth(request, env, ctx, url);
	if (p === '/api/comments' || p.startsWith('/api/comments/')) return handleComments(request, env, url);
	if (p.startsWith('/api/')) return json({ error: 'Not found.' }, 404);
	if (p.startsWith('/audio/')) return handleAudio(request, env, url);
	return (await handlePage(request, env, ctx, url)) ?? assetHeaders(await env.ASSETS.fetch(request));
}

export default {
	async fetch(request, env, ctx) {
		try {
			const inner = enter(request);
			return inner instanceof Response ? inner : await route(inner, env, ctx);
		} catch (e) {
			console.error('unhandled', request.method, new URL(request.url).pathname, e instanceof Error ? e.stack : String(e));
			return new URL(request.url).pathname.startsWith('/api/') ? json({ error: 'Server error.' }, 500) : plain('Server error.', 500);
		}
	},
} satisfies ExportedHandler<Env>;

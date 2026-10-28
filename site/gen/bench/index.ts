// /bench's charts, from the benchmark history. The page names where they go and where the history is
// (`BenchHistory: {dir}` in content/bench.flecs); this writes them as a Flecs script of Sections, Pickers and charts
// (BarChart, LineChart: content/schema.flecs) under that entity, which gen/build.ts runs after the content. The
// renderer draws them like any other block. The subjects charts compare (subjects.blaze, ...) are defined on the page.
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { backendOrder, MAX_RUNS, parseHistory, platformOrder, projectOrder, ranked, type BuildMachine, type Entry, type LiveMachine, type Run } from './data';
import { BACKEND_NAMES, commitUrl, EDIT_ORDER, EDITS, MODE_ORDER, MODES, PROGRAM_ORDER, PROGRAMS, projectName, SETUPS, short, when } from './format';
import { ent, script, slug, type Ent } from './flecs';

/** The platform /bench opens on. */
const FIRST_PLATFORM = 'Linux x86-64';

const plots = (subject: string) => `(Plots, subjects.${subject})`;
const unique = <T>(xs: T[]) => [...new Set(xs)];
const project = (key: string) => key.split('/')[0];
const backend = (key: string) => key.split('/')[1] ?? '';
const machineName = (m: string) => m || 'Unnamed machine';

function runLine(run: Run) {
	const cpu = [run.cpu?.replace(/( \d+-Core)? Processor$/, ''), run.cores ? `${run.cores} cores` : undefined].filter(Boolean).join(', ');
	const commit = run.commit ? `[\`${short(run.commit)}\`](${commitUrl(run.commit)})` : '(commit unknown)';
	return (
		`${machineName(run.machine)}${cpu ? ` (${cpu})` : ''}, blaze ${commit} on ${when(run.date)}` +
		`${run.runs ? `, median of ${run.runs} edits` : ''}.${run.runUrl ? ` [The CI run](${run.runUrl}).` : ''}`
	);
}

/** A Picker over `items`, one Pick each; `picked` names the one it starts on. */
function picker(name: string, label: string, items: { name: string; label: string; body: Ent[] }[], picked?: string): Ent {
	return ent('Picker', name, { label }, items.map((it) => ent('Pick', slug(it.name), { label: it.label }, [...(it.name === picked ? ['Picked'] : []), ...it.body])));
}

/** The fastest other cargo setup on a phase (not `check`, which builds no program), as [seconds, which]. */
function fastestSetup(setups: Record<string, number> | undefined): [number, string] | undefined {
	const all = Object.entries(setups ?? {}).filter(([s]) => s !== 'check' && s in SETUPS);
	if (!all.length) return undefined;
	const [name, v] = all.reduce((a, b) => (b[1] < a[1] ? b : a));
	return [v, SETUPS[name]];
}

const bar = (subject: string, value: number, note = '') => ent('Bar', subject, { value, note }, [plots(subject)]);

/** One project's edits in one run: blaze and cargo per edit, the speedup on each row. */
function editsChart(key: string, e: Entry): Ent | null {
	const rows = Object.keys(e.phases)
		.filter((p) => p !== 'cold')
		.sort(ranked(EDIT_ORDER))
		.flatMap((p) => {
			const ph = e.phases[p];
			const bars: Ent[] = [];
			if (ph.wall_s !== undefined) bars.push(bar('blaze', ph.wall_s, ph.built && ph.built !== 'daemon' ? 'the daemon handed this edit to cargo, so it took about cargo\'s time' : ''));
			if (ph.cargo_s !== undefined) bars.push(bar('cargo', ph.cargo_s));
			const setup = fastestSetup(ph.setups);
			if (setup) bars.push(bar('setup', setup[0], `the fastest other setup here is ${setup[1]}`));
			return bars.length ? [ent('Row', slug(p), { label: EDITS[p] ?? p }, bars)] : [];
		});
	if (!rows.length) return null;
	return ent('BarChart', slug(project(key)), { title: projectName(project(key), e.tag), unit: 's', note: '' }, ['(Baseline, subjects.cargo)', ...rows]);
}

/** Every project of one run on one chart: a row per project. */
function acrossChart(name: string, title: string, unit: string, note: string, entries: [string, Entry][], bars: (e: Entry) => Ent[], baseline = false): Ent | null {
	const rows = entries.flatMap(([key, e]) => {
		const b = bars(e);
		return b.length ? [ent('Row', slug(project(key)), { label: projectName(project(key), e.tag) }, b)] : [];
	});
	return rows.length ? ent('BarChart', name, { title, unit, note }, [...(baseline ? ['(Baseline, subjects.cargo)'] : []), ...rows]) : null;
}

const peak = (e: Entry, field: 'mem_mb' | 'cargo_mem_mb') => {
	const all = Object.entries(e.phases).filter(([p]) => p !== 'cold').flatMap(([, ph]) => (ph[field] === undefined ? [] : [ph[field]!]));
	return all.length ? Math.max(...all) : undefined;
};

/** The newest run of each platform: a Picker of platforms, then of backends. */
function latest(build: BuildMachine[]): Ent[] {
	const newest = new Map<string, BuildMachine['runs'][number]>();
	for (const m of build) {
		const last = m.runs.at(-1)!;
		const had = newest.get(m.platform);
		if (!had || Date.parse(had.run.date) < Date.parse(last.run.date)) newest.set(m.platform, last);
	}
	const platforms = [...newest.keys()].sort(platformOrder);
	if (!platforms.length) return [ent('Dim', 'none', { text: 'No benchmark runs recorded yet.' })];
	const items = platforms.map((platform) => {
		const { run, results } = newest.get(platform)!;
		const keys = Object.keys(results).sort((a, b) => projectOrder(project(a), project(b)) || backendOrder(backend(a), backend(b)));
		const backends = unique(keys.map(backend)).sort(backendOrder).map((b) => {
			const entries = keys.filter((k) => backend(k) === b).map((k) => [k, results[k]] as [string, Entry]);
			const charts = [
				acrossChart('cold', 'Cold build, until ready', 's', 'From nothing built to ready: cargo builds everything; blaze builds and starts its daemon. A second checkout of the same commit starts from the store the first one filled.', entries, (e) => {
					const c = e.phases.cold;
					if (!c) return [];
					return [
						...(c.wall_s !== undefined ? [bar('blaze', c.wall_s)] : []),
						...(c.warm_store_s !== undefined ? [bar('warm', c.warm_store_s)] : []),
						...(c.cargo_s !== undefined ? [bar('cargo', c.cargo_s)] : []),
					];
				}, true),
				acrossChart('memory', 'Memory while editing', 'MB', 'The most every process doing the work held at once over the edits: for blaze the daemon, its workers and the command; for cargo, cargo, rustc and the linker ([how](https://github.com/indexable-inc/blaze/blob/main/docs/benchmarks.md#memory-and-disk)).', entries, (e) => {
					const [ours, theirs] = [peak(e, 'mem_mb'), peak(e, 'cargo_mem_mb')];
					return [...(ours !== undefined ? [bar('blaze', ours)] : []), ...(theirs !== undefined ? [bar('cargo', theirs)] : [])];
				}),
				acrossChart('disk', 'On disk', 'MB', 'blaze: its target directory, store and home. cargo: its target directory.', entries, (e) => [
					...(e.disk_mb !== undefined ? [bar('blaze', e.disk_mb)] : []),
					...(e.cargo_disk_mb !== undefined ? [bar('cargo', e.cargo_disk_mb)] : []),
				]),
			].filter((c): c is Ent => c !== null);
			const edits = entries.map(([k, e]) => editsChart(k, e)).filter((c): c is Ent => c !== null);
			return {
				name: b,
				label: BACKEND_NAMES[b] ?? b,
				body: [
					ent('H3', 'edits', { text: 'Edit, then build' }),
					ent('Charts', 'projects', undefined, edits),
					ent('H3', 'projects_side_by_side', { text: 'Every project' }),
					ent('Charts', 'across', undefined, charts),
				],
			};
		});
		return {
			name: platform,
			label: platform,
			body: [ent('Dim', 'run', { text: runLine(run) }), picker('backend', 'Backend', backends, backends[0]?.name)],
		};
	});
	return [
		ent('P', 'how', { text: "Each bar is the median of the run's edits; the label on the right of an edit is how much faster blaze was than plain cargo. Open a chart's numbers for the table." }),
		picker('platform', 'Platform', items, platforms.includes(FIRST_PLATFORM) ? FIRST_PLATFORM : platforms.at(-1)),
	];
}

/** A series' values as Series writes them: one per run, `-` where there is none. */
const values = (xs: (number | undefined)[]) => xs.map((v) => (v === undefined || !Number.isFinite(v) ? '-' : String(v))).join(' ');
const runs = (rs: { run: Run }[]) => `Runs: {dates: "${rs.map((r) => r.run.date).join(' ')}", commits: "${rs.map((r) => r.run.commit ?? '-').join(' ')}"}`;
const series = (subject: string, xs: (number | undefined)[]) => ent('Series', subject, { values: values(xs), lo: '', hi: '' }, [plots(subject)]);

/** The geometric mean of the edits a run measured, over the edits every one of `entries` measured. */
function editMean(entries: (Entry | undefined)[], field: 'wall_s' | 'cargo_s') {
	const sets = entries.filter((e): e is Entry => !!e).map((e) => Object.keys(e.phases).filter((p) => p !== 'cold' && e.phases[p][field] !== undefined));
	const common = sets.length ? sets.reduce((a, b) => a.filter((p) => b.includes(p))) : [];
	return entries.map((e) => {
		if (!e || !common.length) return undefined;
		const logs = common.map((p) => Math.log(e.phases[p][field]!));
		return Math.exp(logs.reduce((a, b) => a + b, 0) / logs.length);
	});
}

/** Each machine's runs over time: a Picker of machines, then of backends, then a chart per project. */
function overTime(build: BuildMachine[]): Ent[] {
	if (!build.length) return [ent('Dim', 'none', { text: 'No benchmark runs recorded yet.' })];
	const newestOf = (platform: string) => build.filter((m) => m.platform === platform).at(-1);
	const first = newestOf(FIRST_PLATFORM) ?? build.at(-1)!;
	const items = build.map((m) => {
		const keys = unique(m.runs.flatMap((r) => Object.keys(r.results))).sort((a, b) => projectOrder(project(a), project(b)));
		const backends = unique(keys.map(backend)).sort(backendOrder).map((b) => {
			const ofBackend = keys.filter((k) => backend(k) === b);
			const tag = (k: string) => m.runs.map((r) => r.results[k]?.tag).findLast(Boolean);
			const edits = ofBackend.map((k) => {
				const entries = m.runs.map((r) => r.results[k]);
				return ent('LineChart', slug(project(k)), { title: projectName(project(k), tag(k)), unit: 's', note: '' }, [
					runs(m.runs),
					series('blaze', editMean(entries, 'wall_s')),
					series('cargo', editMean(entries, 'cargo_s')),
				]);
			});
			const cold = ofBackend.flatMap((k) => {
				const entries = m.runs.map((r) => r.results[k]?.phases.cold);
				if (!entries.some((c) => c?.wall_s !== undefined || c?.cargo_s !== undefined)) return [];
				return [ent('LineChart', slug(project(k)), { title: projectName(project(k), tag(k)), unit: 's', note: '' }, [
					runs(m.runs),
					series('blaze', entries.map((c) => c?.wall_s)),
					...(entries.some((c) => c?.warm_store_s !== undefined) ? [series('warm', entries.map((c) => c?.warm_store_s))] : []),
					series('cargo', entries.map((c) => c?.cargo_s)),
				])];
			});
			return {
				name: b,
				label: BACKEND_NAMES[b] ?? b,
				body: [
					ent('H3', 'edits', { text: 'Edit, then build' }),
					ent('Charts', 'edits_charts', undefined, edits),
					...(cold.length ? [ent('H3', 'cold', { text: 'Cold build, until ready' }), ent('Charts', 'cold_charts', undefined, cold)] : []),
				],
			};
		});
		const [a, z] = [m.runs[0].run, m.runs.at(-1)!.run];
		const span = m.runs.length === 1 ? `One run, on ${when(a.date)}.` : `${m.runs.length} runs, from ${when(a.date)} to ${when(z.date)}.`;
		return {
			name: `${m.platform} ${m.machine}`,
			label: `${m.platform}: ${machineName(m.machine)}`,
			body: [ent('Dim', 'runs', { text: span }), picker('backend', 'Backend', backends, backends[0]?.name)],
		};
	});
	return [
		ent('P', 'how', { text: "A machine's runs, oldest to newest. Edit, then build is the geometric mean of the medians of the edits every run measured, so one number per run that moves when any edit does; each edit's own numbers are in the latest run above. Point at a chart to read a run." }),
		picker('machine', 'Machine', items, `${first.platform} ${first.machine}`),
	];
}

/** Live edits over time: a Picker of machines, then a chart per program and mode. */
function live(machines: LiveMachine[]): Ent[] {
	if (!machines.length) return [ent('Dim', 'none', { text: 'No live-edit runs recorded yet.' })];
	const newest = [...machines].sort((a, b) => Date.parse(a.runs.at(-1)!.run.date) - Date.parse(b.runs.at(-1)!.run.date)).at(-1)!;
	const items = machines.map((m) => {
		const programs = unique(m.runs.flatMap((r) => Object.keys(r.results))).sort(ranked(PROGRAM_ORDER));
		const body: Ent[] = [ent('Dim', 'runs', { text: m.runs.length === 1 ? `One run, on ${when(m.runs[0].run.date)}.` : `${m.runs.length} runs, from ${when(m.runs[0].run.date)} to ${when(m.runs.at(-1)!.run.date)}.` })];
		for (const program of programs) {
			const modes = unique(m.runs.flatMap((r) => Object.keys(r.results[program] ?? {}))).sort(ranked(MODE_ORDER));
			const charts = modes.map((mode) => {
				const stat = m.runs.map((r) => r.results[program]?.[mode]);
				return ent('LineChart', slug(mode), { title: MODES[mode] ?? mode, unit: 'ms', note: '' }, [
					runs(m.runs),
					series('median', stat.map((s) => s?.p50)),
					series('p90', stat.map((s) => s?.p90)),
				]);
			});
			body.push(ent('H3', slug(program), { text: PROGRAMS[program] ?? program }), ent('Charts', `${slug(program)}_charts`, undefined, charts));
		}
		return { name: m.machine || 'unnamed', label: machineName(m.machine), body };
	});
	return [
		ent('P', 'how', { text: 'Edit to running: from the edit leaving the editor to the new code running in the program. Solid is the median edit, dashed the 90th percentile.' }),
		picker('machine', 'Machine', items, newest.machine || 'unnamed'),
	];
}

/** The history in `dir` as Flecs script under the entity at `at` (a BenchHistory). */
export function benchScript(dir: string, at: string): string {
	const files = readdirSync(dir).filter((f) => f.endsWith('.jsonl')).sort().map((f) => readFileSync(path.join(dir, f), 'utf8'));
	const h = parseHistory(files);
	const capped = [...h.build, ...h.live].some((m) => m.runs.length >= MAX_RUNS);
	return script(at, [
		ent('Section', 'latest', { title: 'Latest run' }, latest(h.build)),
		ent('Section', 'time', { title: 'Over time' }, [
			...overTime(h.build),
			...(capped ? [ent('Dim', 'capped', { text: `The charts show the last ${MAX_RUNS} runs of each machine.` })] : []),
		]),
		ent('Section', 'live', { title: 'Live edits' }, live(h.live)),
	]);
}

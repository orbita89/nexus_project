#!/usr/bin/env node
// Нагрузочный тест WebSocket: открыть N гостевых соединений, подписать каждое на канал и
// замерить память процесса nexus до и после — реальная цена одного соединения.
//
//   node scripts/ws-load.mjs [N=2000] [URL=ws://localhost/ws] [CONTAINER=nexus-app]
//
// Соединения открываются пачками по 200 (без лавины рукопожатий), держатся HOLD_SECONDS
// (по умолчанию 10) и закрываются. Память — VmRSS процесса nexus внутри контейнера
// (`docker exec <CONTAINER> grep VmRSS /proc/<pid>/status`). Node 22+ (глобальный WebSocket).
import { execFileSync } from 'node:child_process';

const N = Number(process.argv[2] ?? 2000);
const URL = process.argv[3] ?? 'ws://localhost/ws';
const CONTAINER = process.argv[4] ?? 'nexus-app';
const HOLD = Number(process.env.HOLD_SECONDS ?? 10);
const BATCH = 200;

function rssKb() {
	const out = execFileSync(
		'docker',
		['exec', CONTAINER, 'sh', '-c', 'grep VmRSS /proc/$(pidof nexus)/status'],
		{ encoding: 'utf8' }
	);
	return Number(out.match(/(\d+)\s*kB/)[1]);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function open() {
	return new Promise((resolve) => {
		const ws = new WebSocket(URL);
		const done = (ok) => resolve({ ws, ok });
		ws.onopen = () => ws.send(JSON.stringify({ type: 'subscribe', channels: ['rooms'] }));
		ws.onmessage = (e) => {
			const m = JSON.parse(e.data);
			if (m.type === 'subscribed') done(true);
		};
		ws.onerror = () => done(false);
		setTimeout(() => done(false), 15_000);
	});
}

const before = rssKb();
console.log(`RSS до: ${(before / 1024).toFixed(1)} МБ`);
const sockets = [];
let failed = 0;
const t0 = Date.now();
for (let i = 0; i < N; i += BATCH) {
	const batch = await Promise.all(Array.from({ length: Math.min(BATCH, N - i) }, open));
	for (const { ws, ok } of batch) ok ? sockets.push(ws) : (failed++, ws.close());
}
console.log(`открыто: ${sockets.length}, ошибок: ${failed}, за ${((Date.now() - t0) / 1000).toFixed(1)} с`);
await sleep(HOLD * 1000);
const during = rssKb();
const perConn = sockets.length ? ((during - before) / sockets.length).toFixed(1) : '—';
console.log(`RSS с соединениями: ${(during / 1024).toFixed(1)} МБ → ~${perConn} КБ на соединение`);
for (const ws of sockets) ws.close();
await sleep(5000);
console.log(`RSS после закрытия: ${(rssKb() / 1024).toFixed(1)} МБ`);
process.exit(0);

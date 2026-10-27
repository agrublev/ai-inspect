import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { HooksManager } from '@openrouter/agent';
import {
	HttpEventSink,
	JsonlFileSink,
	OpenRouterTrace,
	sanitizeTraceData,
} from '../dist/index.js';

function makeResult() {
	let streamReads = 0;
	const result = {
		get streamReads() {
			return streamReads;
		},
		async *getFullResponsesStream() {
			streamReads += 1;
			yield { type: 'turn.start', turnNumber: 0, timestamp: Date.parse('2026-10-27T10:00:00Z') };
			yield {
				type: 'response.output_item.added',
				item: {
					type: 'function_call',
					id: 'item-1',
					call_id: 'call-1',
					name: 'lookup',
					arguments: '{"query":"version"}',
				},
				timestamp: Date.parse('2026-10-27T10:00:01Z'),
			};
			yield {
				type: 'response.function_call_arguments.done',
				call_id: 'call-1',
				name: 'lookup',
				arguments: '{"query":"version"}',
				timestamp: Date.parse('2026-10-27T10:00:02Z'),
			};
			yield {
				type: 'tool.result',
				toolCallId: 'call-1',
				toolName: 'lookup',
				source: 'client',
				result: { version: '1.2.3' },
				timestamp: Date.parse('2026-10-27T10:00:03Z'),
			};
			yield {
				type: 'response.completed',
				response: { id: 'response-1', output: [{ type: 'message', text: '1.2.3' }] },
				timestamp: Date.parse('2026-10-27T10:00:04Z'),
			};
			yield { type: 'turn.end', turnNumber: 0, timestamp: Date.parse('2026-10-27T10:00:05Z') };
		},
		async getText() {
			return '1.2.3';
		},
		async getResponse() {
			return { id: 'response-1', output: [{ type: 'message', text: '1.2.3' }] };
		},
		async getUsage() {
			return { modelCalls: 1, inputTokens: 3, outputTokens: 2, totalTokens: 5 };
		},
	};
	return result;
}

function makeClient(result, onCall = () => {}) {
	return {
		callModel(request) {
			onCall(request);
			return result;
		},
	};
}

test('captures model turns and correlated tool input/results without eager execution', async () => {
	const events = [];
	const result = makeResult();
	const trace = new OpenRouterTrace(makeClient(result), {
		sink: { write: (event) => events.push(event) },
		sessionId: 'agent-session-1',
	});
	const run = trace.client.callModel({
		model: 'openai/gpt-4.1',
		input: 'find version',
		context: { apiKey: 'must-not-appear-in-the-trace' },
	});
	assert.equal(result.streamReads, 0, 'callModel remains lazy until a result consumer is used');
	assert.equal(await run.getText(), '1.2.3');
	await trace.flush();

	assert.equal(result.streamReads, 1);
	assert.ok(events.some((event) => event.event_type === 'trace.input'));
	assert.ok(events.some((event) => event.event_type === 'model.start'));
	assert.ok(events.some((event) => event.event_type === 'tool.start' && event.input === '{"query":"version"}'));
	assert.ok(events.some((event) => event.event_type === 'tool.result' && event.output.version === '1.2.3'));
	assert.ok(events.some((event) => event.event_type === 'model.response'));
	assert.equal(events.at(-1).event_type, 'trace.end');
	const traceInput = events.find((event) => event.event_type === 'trace.input');
	assert.equal(traceInput.session_id, 'agent-session-1');
	assert.equal(traceInput.input.context.apiKey, '[REDACTED]');
	const toolResult = events.find((event) => event.event_type === 'tool.result');
	assert.equal(toolResult.parent_span_id, 'turn-0');
	assert.equal(toolResult.span_id, 'tool-call-1');
});

test('captures SDK hook payloads and preserves hook outcomes', async () => {
	const events = [];
	const hooks = new HooksManager();
	let hookHandlerRan = false;
	hooks.on('PostModelCall', {
		handler: (payload) => {
			hookHandlerRan = payload.responseId === 'response-1';
		},
	});
	let requestHooks;
	const fixture = makeResult();
	const result = {
		...fixture,
		async getText() {
			const emitted = await requestHooks.emit('PostModelCall', {
				sessionId: 'session-1',
				responseId: 'response-1',
				model: 'openai/gpt-4.1',
				durationMs: 12,
				turnType: 'initial',
				turnNumber: 0,
				usage: { inputTokens: 3, outputTokens: 2, totalTokens: 5, cachedTokens: 0, reasoningTokens: 0 },
			});
			assert.equal(emitted.finalPayload.responseId, 'response-1');
			return '1.2.3';
		},
	};
	const trace = new OpenRouterTrace(makeClient(result, (request) => {
		requestHooks = request.hooks;
	}), { sink: { write: (event) => events.push(event) } });
	const run = trace.client.callModel({ model: 'openai/gpt-4.1', input: 'find version', hooks });
	assert.equal(await run.getText(), '1.2.3');
	await trace.flush();

	const hookEvents = events.filter((event) => event.event_type.startsWith('agent.hook.'));
	assert.equal(hookEvents.length, 2);
	assert.equal(hookEvents[0].name, 'Hook PostModelCall');
	assert.equal(hookEvents[1].status, 'ok');
	assert.equal(hookHandlerRan, true);
});

test('redacts credentials and stores JSONL with owner-only file permissions', async () => {
	assert.deepEqual(sanitizeTraceData({ apiKey: 'private', inputTokens: 10, nested: { access_token: 'secret' } }), {
		apiKey: '[REDACTED]',
		inputTokens: 10,
		nested: { access_token: '[REDACTED]' },
	});

	const directory = await mkdtemp(join(tmpdir(), 'iris-openrouter-'));
	const path = join(directory, 'logs', 'events.jsonl');
	const sink = new JsonlFileSink(path);
	const event = {
		source_id: 'run:0', session_id: 'run', trace_id: 'run', span_id: null, parent_span_id: null,
		timestamp: '2026-10-27T10:00:00.000Z', event_type: 'trace.input', name: 'test', status: 'running',
		input: null, output: null, metadata: {},
	};
	try {
		await sink.write(event);
		await sink.flush();
		assert.deepEqual(JSON.parse((await readFile(path, 'utf8')).trim()), event);
		assert.equal((await stat(path)).mode & 0o777, 0o600);
		await sink.close();
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

test('HTTP delivery is loopback-only and does not change agent results on sink failure', async () => {
	assert.throws(() => new HttpEventSink('https://example.com/api/events'), /loopback/);
	let delivered;
	const httpSink = new HttpEventSink(undefined, {
		fetch: async (url, init) => {
			delivered = { url: String(url), init };
			return new Response(null, { status: 201 });
		},
	});
	const event = {
		source_id: 'run:0', session_id: 'run', trace_id: 'run', span_id: null, parent_span_id: null,
		timestamp: '2026-10-27T10:00:00.000Z', event_type: 'trace.input', name: 'test', status: 'running',
		input: null, output: null, metadata: {},
	};
	await httpSink.write(event);
	assert.equal(delivered.url, 'http://127.0.0.1:4318/api/events');
	assert.equal(delivered.init.method, 'POST');
	assert.equal(delivered.init.signal.aborted, false);

	const trace = new OpenRouterTrace(makeClient(makeResult()), {
		sink: { write: async () => { throw new Error('disk unavailable'); } },
	});
	const run = trace.client.callModel({ model: 'openai/gpt-4.1', input: 'find version' });
	assert.equal(await run.getText(), '1.2.3');
	await assert.rejects(trace.flush(), AggregateError);
});

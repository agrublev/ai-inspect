import { randomUUID } from 'node:crypto';
import { createWriteStream, type WriteStream } from 'node:fs';
import { chmod, mkdir } from 'node:fs/promises';
import { homedir } from 'node:os';
import { dirname, resolve } from 'node:path';
import type {
	CallModelInput,
	HookEntry,
	InlineHookConfig,
	ModelResult,
	OpenRouter,
	RequestOptions,
	Tool,
	TurnContext,
} from '@openrouter/agent';
import { HooksManager } from '@openrouter/agent';

export type JsonValue =
	| null
	| boolean
	| number
	| string
	| JsonValue[]
	| { [key: string]: JsonValue };

export interface IrisTraceEvent {
	source_id: string;
	session_id: string;
	trace_id: string;
	span_id: string | null;
	parent_span_id: string | null;
	timestamp: string;
	event_type: string;
	name: string;
	status: string;
	input: JsonValue;
	output: JsonValue;
	metadata: { [key: string]: JsonValue };
}

export interface EventSink {
	write(event: IrisTraceEvent): void | Promise<void>;
	flush?(): void | Promise<void>;
	close?(): void | Promise<void>;
}

export interface OpenRouterTraceOptions {
	sink?: EventSink;
	sessionId?: string;
}

export interface HttpEventSinkOptions {
	fetch?: typeof fetch;
	timeoutMs?: number;
}

interface CaptureResult {
	getFullResponsesStream(): AsyncIterableIterator<unknown>;
	getResponse(): Promise<unknown>;
	getUsage(): Promise<unknown>;
}

const REDACTED = '[REDACTED]';
const REDACTED_KEYS = new Set([
	'authorization',
	'apikey',
	'accesstoken',
	'refreshtoken',
	'password',
	'secret',
	'clientsecret',
	'credential',
]);

const STREAM_METHODS = new Set([
	'getFullResponsesStream',
	'getTextStream',
	'getItemsStream',
	'getNewMessagesStream',
	'getReasoningStream',
	'getToolStream',
	'getToolCallsStream',
	'getContextUpdates',
]);

const EXECUTION_METHODS = new Set([
	'getResponse',
	'getText',
	'getToolCalls',
	'requiresApproval',
	'getPendingToolCalls',
	'getState',
	'getDoomLoopVerdict',
	'getUsage',
]);

export function defaultEventLogPath(): string {
	return resolve(homedir(), '.claude', 'iris', 'openrouter-events.jsonl');
}

export function sanitizeTraceData(value: unknown): JsonValue {
	return sanitize(value, new WeakSet<object>());
}

function sanitize(value: unknown, seen: WeakSet<object>): JsonValue {
	if (value === null || value === undefined) return null;
	if (typeof value === 'string' || typeof value === 'boolean') return value;
	if (typeof value === 'number') return Number.isFinite(value) ? value : String(value);
	if (typeof value === 'bigint') return `${value.toString()}n`;
	if (typeof value === 'function') return '[Function]';
	if (typeof value === 'symbol') return value.toString();
	if (value instanceof Date) return value.toISOString();
	if (value instanceof Error) {
		return {
			name: value.name,
			message: value.message,
			...(value.stack ? { stack: value.stack } : {}),
			...(value.cause !== undefined ? { cause: sanitize(value.cause, seen) } : {}),
		};
	}
	if (typeof value !== 'object') return String(value);
	if (seen.has(value)) return '[Circular]';
	seen.add(value);
	try {
		if (Array.isArray(value)) return value.map((item) => sanitize(item, seen));
		const output: { [key: string]: JsonValue } = {};
		for (const [key, item] of Object.entries(value)) {
			if (isSensitiveKey(key)) {
				output[key] = REDACTED;
			} else {
				try {
					output[key] = sanitize(item, seen);
				} catch {
					output[key] = '[Unserializable]';
				}
			}
		}
		return output;
	} catch {
		return '[Unserializable]';
	} finally {
		seen.delete(value);
	}
}

function isSensitiveKey(key: string): boolean {
	const normalized = key.toLowerCase().replace(/[^a-z0-9]/g, '');
	return REDACTED_KEYS.has(normalized)
		|| normalized.endsWith('apikey')
		|| normalized.endsWith('accesstoken')
		|| normalized.endsWith('refreshtoken')
		|| normalized.endsWith('password')
		|| normalized.endsWith('secret')
		|| normalized.endsWith('credential');
}

export class JsonlFileSink implements EventSink {
	readonly path: string;
	private stream: WriteStream | undefined;
	private opening: Promise<void> | undefined;
	private streamError?: Error;

	constructor(path = defaultEventLogPath()) {
		this.path = resolve(path);
	}

	async write(event: IrisTraceEvent): Promise<void> {
		await this.ensureStream();
		const stream = this.stream;
		if (!stream) throw new Error('trace log stream was not initialized');
		if (this.streamError) throw this.streamError;
		await new Promise<void>((resolveWrite, rejectWrite) => {
			stream.write(`${JSON.stringify(event)}\n`, (error) => {
				if (error) rejectWrite(error);
				else resolveWrite();
			});
		});
	}

	async flush(): Promise<void> {
		if (this.opening) await this.opening;
		if (this.streamError) throw this.streamError;
		if (!this.stream) return;
		await new Promise<void>((resolveWrite, rejectWrite) => {
			this.stream?.write('', (error) => {
				if (error) rejectWrite(error);
				else resolveWrite();
			});
		});
	}

	async close(): Promise<void> {
		await this.flush();
		if (!this.stream) return;
		const stream = this.stream;
		await new Promise<void>((resolveEnd, rejectEnd) => {
			stream.once('error', rejectEnd);
			stream.end(resolveEnd);
		});
		this.stream = undefined;
	}

	private async ensureStream(): Promise<void> {
		if (this.stream) return;
		if (this.opening) return this.opening;
		this.opening = (async () => {
			await mkdir(dirname(this.path), { recursive: true, mode: 0o700 });
			const stream = createWriteStream(this.path, { flags: 'a', mode: 0o600 });
			stream.on('error', (error: Error) => {
				this.streamError = error;
			});
			await new Promise<void>((resolveOpen, rejectOpen) => {
				stream.once('open', () => resolveOpen());
				stream.once('error', rejectOpen);
			});
			await chmod(this.path, 0o600);
			this.stream = stream;
		})();
		try {
			await this.opening;
		} finally {
			this.opening = undefined;
		}
	}
}

export class HttpEventSink implements EventSink {
	private readonly endpoint: URL;
	private readonly fetcher: typeof fetch;
	private readonly timeoutMs: number;

	constructor(endpoint = 'http://127.0.0.1:4318/api/events', options: HttpEventSinkOptions = {}) {
		this.endpoint = new URL(endpoint);
		if (this.endpoint.protocol !== 'http:' || !isLoopbackHost(this.endpoint.hostname)) {
			throw new TypeError('trace event delivery is restricted to an HTTP loopback address');
		}
		if (this.endpoint.username || this.endpoint.password || this.endpoint.search || this.endpoint.hash) {
			throw new TypeError('trace event endpoint cannot contain credentials, query parameters, or a fragment');
		}
		this.fetcher = options.fetch ?? fetch;
		this.timeoutMs = options.timeoutMs ?? 5_000;
		if (!Number.isFinite(this.timeoutMs) || this.timeoutMs <= 0) {
			throw new TypeError('trace event timeout must be a positive finite number');
		}
	}

	async write(event: IrisTraceEvent): Promise<void> {
		const response = await this.fetcher(this.endpoint, {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify(event),
			redirect: 'error',
			signal: AbortSignal.timeout(this.timeoutMs),
		});
		if (!response.ok) {
			const detail = await response.text().catch(() => '');
			throw new Error(`Iris event delivery failed (${response.status})${detail ? `: ${detail}` : ''}`);
		}
	}
}

function isLoopbackHost(hostname: string): boolean {
	return hostname === '127.0.0.1' || hostname === '[::1]' || hostname === '::1';
}

export class CompositeEventSink implements EventSink {
	constructor(private readonly sinks: readonly EventSink[]) {
		if (sinks.length === 0) throw new TypeError('CompositeEventSink requires at least one sink');
	}

	async write(event: IrisTraceEvent): Promise<void> {
		await Promise.all(this.sinks.map((sink) => sink.write(event)));
	}

	async flush(): Promise<void> {
		await Promise.all(this.sinks.map((sink) => sink.flush?.()));
	}

	async close(): Promise<void> {
		await Promise.all(this.sinks.map((sink) => sink.close?.()));
	}
}

class EventWriter {
	private tail: Promise<void> = Promise.resolve();
	private readonly errors: Error[] = [];
	private errorCount = 0;
	private static readonly maxRecordedErrors = 20;

	constructor(private readonly sink: EventSink) {}

	write(event: IrisTraceEvent): void {
		this.tail = this.tail.then(async () => this.sink.write(event)).catch((error: unknown) => {
			this.recordError(error);
		});
	}

	async flush(): Promise<void> {
		await this.tail;
		try {
			await this.sink.flush?.();
		} catch (error) {
			this.recordError(error);
		}
		if (this.errorCount > 0) {
			throw new AggregateError(
				this.errors,
				`${this.errorCount} trace event write(s) could not be persisted; showing up to ${EventWriter.maxRecordedErrors} errors`,
			);
		}
	}

	async close(): Promise<void> {
		await this.flush();
		await this.sink.close?.();
	}

	private recordError(error: unknown): void {
		this.errorCount += 1;
		if (this.errors.length < EventWriter.maxRecordedErrors) this.errors.push(toError(error));
	}
}

interface EventFields {
	eventType: string;
	name: string;
	status: string;
	spanId?: string | null;
	parentSpanId?: string | null;
	input?: unknown;
	output?: unknown;
	timestamp?: string;
	metadata?: Record<string, unknown>;
}

class RunCapture {
	readonly traceId = randomUUID();
	private sequence = 0;
	private started = false;
	private ended = false;
	private currentTurn = 0;
	private readonly toolNames = new Map<string, string>();

	constructor(
		private readonly writer: EventWriter,
		private readonly onObserver: (observer: Promise<void>) => void,
		private readonly request: CallModelInput,
		private readonly sessionId: string,
	) {}

	start(result: CaptureResult): void {
		if (this.started) return;
		this.started = true;
		this.record({
			eventType: 'trace.input',
			name: 'OpenRouter agent run',
			status: 'running',
			spanId: this.traceId,
			input: summarizeRequest(this.request),
			metadata: { source: 'openrouter-agent', sdk_version: '0.11.0' },
		});
		const observer = this.observe(result);
		this.onObserver(observer);
	}

	recordHookStart(hookName: string, payload: unknown): string {
		const spanId = `hook-${this.sequence}`;
		const toolCall = isRecord(payload) ? extractToolCall(payload) : null;
		this.record({
			eventType: 'agent.hook.start',
			name: `Hook ${hookName}`,
			status: 'running',
			spanId,
			parentSpanId: toolCall?.toolCallId ? `tool-${toolCall.toolCallId}` : this.turnSpanId(this.currentTurn),
			input: payload,
			metadata: { hook_name: hookName },
		});
		return spanId;
	}

	recordHookResult(spanId: string, hookName: string, result: unknown): void {
		this.record({
			eventType: 'agent.hook.result',
			name: `Hook ${hookName}`,
			status: 'ok',
			spanId,
			parentSpanId: this.turnSpanId(this.currentTurn),
			output: result,
			metadata: { hook_name: hookName },
		});
	}

	recordHookError(spanId: string, hookName: string, error: unknown): void {
		this.record({
			eventType: 'agent.hook.result',
			name: `Hook ${hookName}`,
			status: 'error',
			spanId,
			parentSpanId: this.turnSpanId(this.currentTurn),
			output: toError(error),
			metadata: { hook_name: hookName },
		});
	}

	recordToolTurnStart(context: TurnContext): void {
		this.record({
			eventType: 'agent.tool_turn.start',
			name: `Tool execution turn ${context.numberOfTurns}`,
			status: 'running',
			spanId: `tool-turn-${context.numberOfTurns}`,
			parentSpanId: this.turnSpanId(this.currentTurn),
			input: context.turnRequest ?? null,
			metadata: { turn_number: context.numberOfTurns },
		});
	}

	recordToolTurnEnd(context: TurnContext, response: unknown): void {
		this.record({
			eventType: 'agent.tool_turn.end',
			name: `Tool execution turn ${context.numberOfTurns}`,
			status: 'ok',
			spanId: `tool-turn-${context.numberOfTurns}`,
			parentSpanId: this.turnSpanId(this.currentTurn),
			output: response,
			metadata: { turn_number: context.numberOfTurns },
		});
	}

	private async observe(result: CaptureResult): Promise<void> {
		let streamError: unknown;
		try {
			for await (const event of result.getFullResponsesStream()) {
				this.recordStreamEvent(event);
			}
		} catch (error) {
			streamError = error;
		}

		const [response, usage] = await Promise.allSettled([result.getResponse(), result.getUsage()]);
		const error = streamError
			?? (response.status === 'rejected' ? response.reason : undefined);
		const finalResponse = response.status === 'fulfilled' ? response.value : null;
		const usageData = usage.status === 'fulfilled' ? usage.value : null;
		this.end(error, {
			response: finalResponse,
			usage: usageData,
			...(error !== undefined ? { error: toError(error) } : {}),
			...(usage.status === 'rejected' ? { usage_error: toError(usage.reason) } : {}),
		});
	}

	private recordStreamEvent(event: unknown): void {
		if (!isRecord(event) || typeof event.type !== 'string') return;
		const eventType = event.type;
		const timestamp = timestampFrom(event.timestamp);
		const turnNumber = typeof event.turnNumber === 'number' ? event.turnNumber : this.currentTurn;
		if (eventType === 'turn.start' || eventType === 'turn.end') this.currentTurn = turnNumber;
		const turnSpanId = this.turnSpanId(this.currentTurn);
		const tool = extractToolCall(event);
		if (tool?.toolCallId && tool.toolName) this.toolNames.set(tool.toolCallId, tool.toolName);

		if (eventType === 'turn.start') {
			this.record({
				eventType: 'model.start',
				name: `Model turn ${turnNumber}`,
				status: 'running',
				spanId: turnSpanId,
				parentSpanId: this.traceId,
				output: event,
				timestamp,
				metadata: { turn_number: turnNumber },
			});
			return;
		}
		if (eventType === 'turn.end') {
			this.record({
				eventType: 'model.turn.end',
				name: `Model turn ${turnNumber}`,
				status: 'ok',
				spanId: turnSpanId,
				parentSpanId: this.traceId,
				output: event,
				timestamp,
				metadata: { turn_number: turnNumber },
			});
			return;
		}

		if (eventType === 'tool.preliminary_result' || eventType === 'tool.result') {
			const result = event.result;
			const failed = eventType === 'tool.result' && isRecord(result) && 'error' in result;
			this.record({
				eventType: eventType === 'tool.result' ? 'tool.result' : 'tool.progress',
				name: tool?.toolName ?? (tool?.toolCallId ? this.toolNames.get(tool.toolCallId) : undefined) ?? 'Agent tool',
				status: failed ? 'error' : eventType === 'tool.result' ? 'ok' : 'running',
				spanId: tool?.toolCallId ? `tool-${tool.toolCallId}` : turnSpanId,
				parentSpanId: turnSpanId,
				input: eventType === 'tool.result' ? null : event,
				output: result,
				timestamp,
				metadata: { source: event.source ?? 'client', turn_number: this.currentTurn },
			});
			return;
		}
		if (eventType === 'tool.async_started' || eventType === 'tool.async_settled') {
			const status = eventType === 'tool.async_started'
				? 'running'
				: event.status === 'completed' ? 'ok' : 'error';
			this.record({
				eventType: eventType === 'tool.async_started' ? 'tool.async_start' : 'tool.async_result',
				name: tool?.toolName ?? this.toolNames.get(tool?.toolCallId ?? '') ?? 'Async tool',
				status,
				spanId: tool?.toolCallId ? `tool-${tool.toolCallId}` : turnSpanId,
				parentSpanId: turnSpanId,
				input: eventType === 'tool.async_started' ? event : null,
				output: eventType === 'tool.async_settled' ? event : null,
				timestamp,
				metadata: { turn_number: this.currentTurn },
			});
			return;
		}
		if (eventType === 'tool.call_output') {
			this.record({
				eventType: 'tool.output',
				name: tool?.toolName ?? 'Tool output',
				status: 'ok',
				spanId: tool?.toolCallId ? `tool-${tool.toolCallId}` : turnSpanId,
				parentSpanId: turnSpanId,
				output: event.output,
				timestamp,
				metadata: { turn_number: this.currentTurn },
			});
			return;
		}
		if (tool && eventType === 'response.output_item.added') {
			this.record({
				eventType: 'tool.start',
				name: tool.toolName ?? 'Agent tool',
				status: 'running',
				spanId: `tool-${tool.toolCallId}`,
				parentSpanId: turnSpanId,
				input: tool.arguments ?? event.item ?? event,
				timestamp,
				metadata: { turn_number: this.currentTurn, sdk_event_type: eventType },
			});
			return;
		}
		if (tool && eventType.includes('function_call_arguments')) {
			this.record({
				eventType: 'tool.arguments',
				name: tool.toolName ?? 'Agent tool',
				status: 'running',
				spanId: `tool-${tool.toolCallId}`,
				parentSpanId: turnSpanId,
				output: event,
				timestamp,
				metadata: { turn_number: this.currentTurn, sdk_event_type: eventType },
			});
			return;
		}

		const modelEvent = eventType === 'response.completed'
			? { eventType: 'model.response', status: 'ok' }
			: eventType === 'response.failed' || eventType === 'response.incomplete'
				? { eventType: 'model.failure', status: 'error' }
				: { eventType: 'model.event', status: 'running' };
		this.record({
			eventType: modelEvent.eventType,
			name: eventType === 'response.completed'
				? `Model response ${turnNumber}`
				: `OpenRouter ${eventType}`,
			status: modelEvent.status,
			spanId: turnSpanId,
			parentSpanId: this.traceId,
			output: event,
			timestamp,
			metadata: { turn_number: turnNumber, sdk_event_type: eventType },
		});
	}

	private end(error: unknown, output: unknown): void {
		if (this.ended) return;
		this.ended = true;
		this.record({
			eventType: 'trace.end',
			name: 'OpenRouter agent run',
			status: error === undefined ? 'ok' : 'error',
			spanId: this.traceId,
			output,
			metadata: { source: 'openrouter-agent', sdk_version: '0.11.0' },
		});
	}

	private record(fields: EventFields): void {
		try {
			const event: IrisTraceEvent = {
				source_id: `${this.traceId}:${this.sequence++}`,
				session_id: this.sessionId,
				trace_id: this.traceId,
				span_id: fields.spanId ?? null,
				parent_span_id: fields.parentSpanId ?? null,
				timestamp: fields.timestamp ?? new Date().toISOString(),
				event_type: fields.eventType,
				name: fields.name,
				status: fields.status,
				input: sanitizeTraceData(fields.input),
				output: sanitizeTraceData(fields.output),
				metadata: sanitizeTraceData({
					source: 'openrouter-agent',
					sdk_version: '0.11.0',
					...fields.metadata,
				}) as { [key: string]: JsonValue },
			};
			this.writer.write(event);
		} catch {
			// Capture failures must never change the agent SDK's behavior.
		}
	}

	private turnSpanId(turnNumber: number): string {
		return `turn-${turnNumber}`;
	}
}

function extractToolCall(event: Record<string, unknown>): {
	toolCallId?: string;
	toolName?: string;
	arguments?: unknown;
} | null {
	if (typeof event.toolCallId === 'string') {
		return {
			toolCallId: event.toolCallId,
			...(typeof event.toolName === 'string' ? { toolName: event.toolName } : {}),
			...('result' in event ? { arguments: event.result } : {}),
		};
	}
	if (typeof event.call_id === 'string' || typeof event.item_id === 'string') {
		const item = isRecord(event.item) ? event.item : undefined;
		return {
			toolCallId: typeof event.call_id === 'string' ? event.call_id : String(event.item_id),
			...(typeof event.name === 'string'
				? { toolName: event.name }
				: item && typeof item.name === 'string'
					? { toolName: item.name }
					: item && typeof item.type === 'string'
						? { toolName: item.type }
						: {}),
			...(item && 'arguments' in item ? { arguments: item.arguments } : {}),
		};
	}
	if (isRecord(event.item) && event.item.type === 'function_call') {
		return {
			...(typeof event.item.call_id === 'string' ? { toolCallId: event.item.call_id } : {}),
			...(typeof event.item.name === 'string' ? { toolName: event.item.name } : {}),
			...('arguments' in event.item ? { arguments: event.item.arguments } : {}),
		};
	}
	return null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function summarizeRequest(request: CallModelInput): unknown {
	const {
		tools,
		hooks,
		onTurnStart,
		onTurnEnd,
		requireApproval,
		signal,
		...input
	} = request;
	const hookNames = hooks instanceof HooksManager
		? [
			'PreToolUse',
			'PostToolUse',
			'PostToolUseFailure',
			'UserPromptSubmit',
			'Stop',
			'PermissionRequest',
			'SessionStart',
			'SessionEnd',
			'PostModelCall',
			'DoomLoopDetected',
		].filter((name) => hooks.hasHandlers(name))
		: hooks ? Object.keys(hooks) : [];
	return {
		...input,
		tools: tools?.map((tool) => {
			if ('function' in tool) {
				return {
					type: tool.type,
					name: tool.function.name,
					description: tool.function.description ?? null,
					client_execution: ('execute' in tool.function
						&& typeof tool.function.execute === 'function')
						|| ('run' in tool.function && typeof tool.function.run === 'function'),
				};
			}
			return { type: tool.config.type, id: tool.id ?? null, config: tool.config };
		}) ?? [],
		hooks: hookNames,
		has_approval_handler: Boolean(requireApproval),
		has_abort_signal: Boolean(signal),
	};
}

function timestampFrom(value: unknown): string {
	if (typeof value === 'string' && !Number.isNaN(Date.parse(value))) {
		return new Date(value).toISOString();
	}
	if (typeof value === 'number' && Number.isFinite(value)) {
		const date = new Date(value);
		if (!Number.isNaN(date.getTime())) return date.toISOString();
	}
	return new Date().toISOString();
}

function toError(value: unknown): Error {
	return value instanceof Error ? value : new Error(String(value));
}

function observeIterator<T>(
	iterator: AsyncIterableIterator<T>,
	start: () => void,
): AsyncIterableIterator<T> {
	return new Proxy(iterator, {
		get(target, property, receiver) {
			if (property === 'next') {
				return (...args: Parameters<typeof target.next>) => {
					start();
					return target.next(...args);
				};
			}
			return Reflect.get(target, property, receiver);
		},
	});
}

function observeResult<TTools extends readonly Tool[]>(
	result: ModelResult<TTools>,
	capture: RunCapture,
): ModelResult<TTools> {
	return new Proxy(result, {
		get(target, property, receiver) {
			const value = Reflect.get(target, property, target);
			if (typeof property !== 'string' || typeof value !== 'function') {
				return value;
			}
			if (STREAM_METHODS.has(property)) {
				return (...args: unknown[]) => observeIterator(
					Reflect.apply(value, target, args) as AsyncIterableIterator<unknown>,
					() => capture.start(result),
				);
			}
			if (EXECUTION_METHODS.has(property)) {
				return (...args: unknown[]) => {
					capture.start(result);
					return Reflect.apply(value, target, args);
				};
			}
			return value.bind(target);
		},
	});
}

function observeHooksManager<T extends HooksManager>(manager: T, capture: RunCapture): T {
	return new Proxy(manager, {
		get(target, property, receiver) {
			const value = Reflect.get(target, property, target);
			if (property !== 'emit' || typeof value !== 'function') {
				return typeof value === 'function' ? value.bind(target) : value;
			}
			return async (...args: unknown[]) => {
				const hookName = typeof args[0] === 'string' ? args[0] : 'unknown';
				const spanId = capture.recordHookStart(hookName, args[1]);
				try {
					const result: unknown = await Reflect.apply(value, target, args);
					capture.recordHookResult(spanId, hookName, result);
					return result;
				} catch (error) {
					capture.recordHookError(spanId, hookName, error);
					throw error;
				}
			};
		},
	});
}

function observeHookEntries<P, R>(
	hookName: string,
	entries: HookEntry<P, R>[],
	capture: RunCapture,
): HookEntry<P, R>[] {
	return entries.map((entry) => ({
		...entry,
		handler: async (payload, context) => {
			const spanId = capture.recordHookStart(hookName, payload);
			try {
				const result = await entry.handler(payload, context);
				capture.recordHookResult(spanId, hookName, result);
				return result;
			} catch (error) {
				capture.recordHookError(spanId, hookName, error);
				throw error;
			}
		},
	}));
}

function observeInlineHooks(hooks: InlineHookConfig, capture: RunCapture): InlineHookConfig {
	return {
		...(hooks.PreToolUse ? { PreToolUse: observeHookEntries('PreToolUse', hooks.PreToolUse, capture) } : {}),
		...(hooks.PostToolUse ? { PostToolUse: observeHookEntries('PostToolUse', hooks.PostToolUse, capture) } : {}),
		...(hooks.PostToolUseFailure ? { PostToolUseFailure: observeHookEntries('PostToolUseFailure', hooks.PostToolUseFailure, capture) } : {}),
		...(hooks.UserPromptSubmit ? { UserPromptSubmit: observeHookEntries('UserPromptSubmit', hooks.UserPromptSubmit, capture) } : {}),
		...(hooks.Stop ? { Stop: observeHookEntries('Stop', hooks.Stop, capture) } : {}),
		...(hooks.PermissionRequest ? { PermissionRequest: observeHookEntries('PermissionRequest', hooks.PermissionRequest, capture) } : {}),
		...(hooks.SessionStart ? { SessionStart: observeHookEntries('SessionStart', hooks.SessionStart, capture) } : {}),
		...(hooks.SessionEnd ? { SessionEnd: observeHookEntries('SessionEnd', hooks.SessionEnd, capture) } : {}),
		...(hooks.PostModelCall ? { PostModelCall: observeHookEntries('PostModelCall', hooks.PostModelCall, capture) } : {}),
		...(hooks.DoomLoopDetected ? { DoomLoopDetected: observeHookEntries('DoomLoopDetected', hooks.DoomLoopDetected, capture) } : {}),
	};
}

function observeHooks(
	hooks: HooksManager | InlineHookConfig,
	capture: RunCapture,
): HooksManager | InlineHookConfig {
	return hooks instanceof HooksManager
		? observeHooksManager(hooks, capture)
		: observeInlineHooks(hooks, capture);
}

export class OpenRouterTrace {
	readonly client: OpenRouter;
	private readonly writer: EventWriter;
	private readonly sessionId: string;
	private readonly activeObservers = new Set<Promise<void>>();

	constructor(client: OpenRouter, options: OpenRouterTraceOptions = {}) {
		this.writer = new EventWriter(options.sink ?? new JsonlFileSink());
		this.sessionId = options.sessionId ?? randomUUID();
		this.client = new Proxy(client, {
			get: (target, property, receiver) => {
				if (property !== 'callModel') return Reflect.get(target, property, receiver);
				return (request: CallModelInput, requestOptions?: RequestOptions) => {
					const capture = new RunCapture(
						this.writer,
						(observer) => {
							this.activeObservers.add(observer);
							void observer.then(
								() => this.activeObservers.delete(observer),
								() => this.activeObservers.delete(observer),
							);
						},
						request,
						this.sessionId,
					);
					const onTurnStart = request.onTurnStart;
				const onTurnEnd = request.onTurnEnd;
				const observedRequest: CallModelInput = {
					...request,
					...(request.hooks ? { hooks: observeHooks(request.hooks, capture) } : {}),
					onTurnStart: async (context) => {
						capture.recordToolTurnStart(context);
						await onTurnStart?.(context);
					},
					onTurnEnd: async (context, response) => {
						capture.recordToolTurnEnd(context, response);
						await onTurnEnd?.(context, response);
					},
				};
				const result = target.callModel(observedRequest, requestOptions);
				return observeResult(result, capture);
				};
			},
		});
	}

	async flush(): Promise<void> {
		while (this.activeObservers.size > 0) {
			await Promise.all([...this.activeObservers]);
		}
		await this.writer.flush();
	}

	async close(): Promise<void> {
		await this.flush();
		await this.writer.close();
	}
}

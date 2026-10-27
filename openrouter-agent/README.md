# Iris OpenRouter Agent wrapper

This package wraps `@openrouter/agent` without changing its call or result types. It records each
run's request, per-turn response stream, tool arguments/progress/results, async task lifecycle,
SDK lifecycle-hook payloads and results, final response, and aggregate usage. Capture starts only
when the caller first consumes the SDK `ModelResult`, so merely constructing a result remains lazy.

## Build and test

```bash
npm ci
npm test
```

The package targets Node.js 20 or later and pins `@openrouter/agent` 0.11.0 in `package-lock.json`.

## Use

```js
import { OpenRouter } from '@openrouter/agent';
import {
  CompositeEventSink,
  HttpEventSink,
  JsonlFileSink,
  OpenRouterTrace,
} from './openrouter-agent/dist/index.js';

const sdk = new OpenRouter({ apiKey: process.env.OPENROUTER_API_KEY });
const tracing = new OpenRouterTrace(sdk, {
  sessionId: 'my-agent-session', // Optional; group runs from the same agent session.
  sink: new CompositeEventSink([
    new JsonlFileSink(),
    new HttpEventSink(), // Optional: delivers to a local `iris web` instance.
  ]),
});

const result = tracing.client.callModel({
  model: 'openai/gpt-4.1',
  input: 'Find the latest release and summarize it.',
  tools: [/* normal @openrouter/agent tools */],
});
console.log(await result.getText());
await tracing.flush();
```

The default append-only JSONL file is `~/.claude/iris/openrouter-events.jsonl`; newly created log
files use mode `0600`. `HttpEventSink` is optional, accepts loopback HTTP only, and posts to Iris'
`POST /api/events` endpoint on `127.0.0.1:4318`. Start `iris web` to make these traces visible in the
existing explorer. HTTP delivery has a five-second timeout by default. If event persistence fails,
the wrapped agent result still behaves normally; `flush()` reports the capture errors afterward.

The recorder redacts common credential-shaped fields and never logs SDK request options or API
headers. Trace payloads still contain prompts, model outputs, tool arguments/results, and possibly
reasoning or user data. Treat the JSONL file and local trace database as sensitive. No remote
telemetry is sent; the only network requests are the OpenRouter calls your agent already makes and,
if explicitly enabled, requests to the loopback Iris server.

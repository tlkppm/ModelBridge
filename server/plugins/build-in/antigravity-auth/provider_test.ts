import type {
  JsonValue,
  NetworkEventStream,
  NetworkResponse,
  PluginContext,
} from "cursor-byok:plugin";
import type { LlmRequest, ModelEvent } from "cursor-byok:provider";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { antigravityProvider, isQuotaError } from "./provider.ts";
import { RESOURCE_TYPE } from "./resources.ts";

function assert(condition: unknown, message = "assertion failed"): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

type RequestInit = { body?: string; headers?: Record<string, string> };
type FetchHandler = (url: string, init?: RequestInit) => NetworkResponse;
type StreamHandler = (url: string, init?: RequestInit) => NetworkEventStream;

function context(handlers: { fetch?: FetchHandler; stream?: StreamHandler }): PluginContext {
  return {
    network: {
      fetch: (url, init) => {
        if (!handlers.fetch) throw new Error("fetch was not expected");
        return Promise.resolve(handlers.fetch(url, init));
      },
      stream: (url, init) => {
        if (!handlers.stream) throw new Error("stream was not expected");
        return Promise.resolve(handlers.stream(url, init));
      },
    },
    signal: new AbortController().signal,
  };
}

function snapshot(privateData: JsonValue): ResourceSnapshot {
  return {
    id: "resource-1",
    type: RESOURCE_TYPE,
    key: "antigravity:user-1",
    privateData,
    state: { status: "ready" },
  };
}

async function* sse(lines: string[]): AsyncGenerator<string> {
  for (const line of lines) yield line;
}

function request(): LlmRequest {
  return {
    instructions: "You are an AI coding assistant.",
    messages: [{ role: "user", content: [{ type: "text", text: "Hello" }] }],
    tools: [],
    reasoning: { enabled: true, effort: "medium" },
    latency: "standard",
    maxOutputTokens: 65536,
    cacheKey: "conv-1",
  };
}

async function invokeMock(lines: AsyncIterable<string>, input = request()) {
  const events: ModelEvent[] = [];
  let requestBody = "";
  let streamCalls = 0;
  const result = await antigravityProvider.invoke(
    {
      model: {
        id: "gemini-3.7-flash-medium",
        displayName: "Gemini 3.7 Flash Medium",
        privateData: {},
      },
      resource: snapshot({ accessToken: "mock-access-token" }),
      request: input,
    },
    { emit: (event) => events.push(event) },
    context({
      stream: (_url, init) => {
        streamCalls++;
        requestBody = init?.body ?? "";
        return { status: 200, headers: {}, lines };
      },
    }),
  );
  const body = JSON.parse(requestBody) as { request: { contents: JsonValue[] } };
  return { events, result, contents: body.request.contents, streamCalls };
}

Deno.test("provider merges split usage metadata before emitting STOP", async () => {
  let requestBody = "";
  let requestHeaders: Record<string, string> = {};
  const events: ModelEvent[] = [];

  const result = await antigravityProvider.invoke(
    {
      model: {
        id: "gemini-3.7-flash-medium",
        displayName: "Gemini 3.7 Flash Medium",
        privateData: {},
      },
      resource: snapshot({
        accessToken: "mock-access-token",
        refreshToken: "mock-refresh-token",
        expiresAt: Date.now() + 3600_000,
        projectId: "test-project-123",
      }),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      stream: (url, init) => {
        assert(url.includes("/v1internal:streamGenerateContent?alt=sse"));
        requestBody = init?.body ?? "";
        requestHeaders = init?.headers ?? {};
        return {
          status: 200,
          headers: {},
          lines: sse([
            'data: {"response":{"candidates":[{"content":{"parts":[{"thought":true,"text":"Thinking..."}]}}],"usageMetadata":{"promptTokenCount":120,"cachedContentTokenCount":80}}}',
            "data: ",
            'data: {"candidates":[{"content":{"parts":[{"text":"Hello world"}]}}]}',
            'data: {"response":{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"candidatesTokenCount":15}}}',
            'data: {"response":{"usageMetadata":{"totalTokenCount":135,"thoughtsTokenCount":0}}}',
            "data: [DONE]",
          ]),
        };
      },
    }),
  );

  assertEquals(result, { status: "completed" });
  assertEquals(requestHeaders["authorization"], "Bearer mock-access-token");
  assertEquals(requestHeaders.accept, "text/event-stream");
  assert(!("session-id" in requestHeaders), "Codex-only session-id header was sent");
  assert(!("thread-id" in requestHeaders), "Codex-only thread-id header was sent");
  assert(!("x-client-request-id" in requestHeaders), "request-id header was reused as cache key");
  const body = JSON.parse(requestBody) as Record<string, unknown>;
  assertEquals(body.project, "test-project-123");
  assertEquals(body.model, "gemini-3.7-flash-medium");
  assert(!("sessionId" in body), "unsupported sessionId field was sent");
  assert(!("conversationId" in body), "unsupported conversationId field was sent");

  assertEquals(events, [
    { type: "thinking-start" },
    { type: "thinking-delta", text: "Thinking..." },
    { type: "thinking-end" },
    { type: "text-start" },
    { type: "text-delta", text: "Hello world" },
    { type: "text-end" },
    {
      type: "usage",
      usage: {
        inputTokens: 120,
        outputTokens: 15,
        totalTokens: 135,
        cacheReadTokens: 80,
        cacheWriteTokens: null,
        reasoningTokens: 0,
      },
    },
    { type: "done", reason: "stop" },
  ]);
});

Deno.test("provider replays same-name tool calls and results by ID with each call's signature", async () => {
  const first = await invokeMock(sse([
    'data: {"response":{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call-a","name":"lookup","args":{"key":"a"}},"thoughtSignature":"signature-a"},{"functionCall":{"id":"call-b","name":"lookup","args":{"key":"b"}}}]}}]}}',
    'data: {"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"lookup","args":{"key":"c"}},"thoughtSignature":"signature-c"}]}}]}}',
    'data: {"response":{"candidates":[{"finishReason":"STOP"}]}}',
  ]));
  const calls = first.events.filter((event) => event.type === "tool-call-start");
  const [firstCall, secondCall, thirdCall] = calls;
  assert(firstCall && secondCall && thirdCall?.callId);
  assertEquals(calls.map((call) => [call.index, call.name]), [
    [0, "lookup"],
    [1, "lookup"],
    [2, "lookup"],
  ]);
  assertEquals([firstCall.callId, secondCall.callId], ["call-a", "call-b"]);
  assertEquals(new Set(calls.map((call) => call.callId)).size, 3);
  assertEquals(first.result, { status: "completed" });
  assertEquals(first.events.filter((event) => event.type === "done"), [
    { type: "done", reason: "tool-use" },
  ]);
  const replayEvents = first.events.filter((event) => event.type === "replay-state");
  assertEquals(replayEvents.length, 1);
  const { providerKind, value } = replayEvents[0];
  const next = request();
  next.messages.push({
    role: "assistant",
    text: "",
    thinking: "",
    replayState: { providerKind, value },
    toolCalls: calls.map((call, index) => ({
      ...call,
      arguments: { key: ["a", "b", "c"][index] },
    })),
  });
  for (const [call, content] of [
    [thirdCall, "result-c"],
    [secondCall, "result-b"],
    [firstCall, "result-a"],
  ] as const) {
    next.messages.push({
      role: "tool",
      callId: call.callId,
      name: "lookup",
      content,
      isError: false,
      parts: [],
    });
  }
  const replay = await invokeMock(sse(['data: {"candidates":[{"finishReason":"STOP"}]}']), next);
  assertEquals(replay.result, { status: "completed" });
  assertEquals(replay.contents.slice(1), [
    {
      role: "model",
      parts: [
        {
          functionCall: { id: "call-a", name: "lookup", args: { key: "a" } },
          thoughtSignature: "signature-a",
        },
        { functionCall: { id: "call-b", name: "lookup", args: { key: "b" } } },
        {
          functionCall: { id: thirdCall.callId, name: "lookup", args: { key: "c" } },
          thoughtSignature: "signature-c",
        },
      ],
    },
    {
      role: "user",
      parts: [
        { functionResponse: { id: "call-a", name: "lookup", response: { result: "result-a" } } },
        { functionResponse: { id: "call-b", name: "lookup", response: { result: "result-b" } } },
        {
          functionResponse: {
            id: thirdCall.callId,
            name: "lookup",
            response: { result: "result-c" },
          },
        },
      ],
    },
  ]);
});

Deno.test("provider keeps scalar and missing-signature fallbacks for older replay state", async () => {
  const input = request();
  for (const [providerKind, signature] of [
    ["antigravity", "legacy-signature"],
    ["other-provider", "foreign-signature"],
  ]) {
    input.messages.push({
      role: "assistant",
      text: "",
      thinking: "",
      replayState: { providerKind, value: { thoughtSignature: signature } },
      toolCalls: [{ index: 0, callId: providerKind, name: "lookup", arguments: {} }],
    }, {
      role: "tool",
      callId: providerKind,
      name: "lookup",
      content: "result",
      isError: false,
      parts: [],
    });
  }
  const { contents, result } = await invokeMock(
    sse(['data: {"candidates":[{"finishReason":"STOP"}]}']),
    input,
  );
  assertEquals(result, { status: "completed" });
  assertEquals(contents[1], {
    role: "model",
    parts: [{
      functionCall: { id: "antigravity", name: "lookup", args: {} },
      thoughtSignature: "legacy-signature",
    }],
  });
  assertEquals(contents[3], {
    role: "model",
    parts: [{
      functionCall: { id: "other-provider", name: "lookup", args: {} },
      thoughtSignature: "skip_thought_signature_validator",
    }],
  });
});

Deno.test("provider reports MAX_TOKENS as length even after a tool call", async () => {
  const { events, result } = await invokeMock(sse([
    'data: {"candidates":[{"content":{"parts":[{"functionCall":{"id":"call-a","name":"lookup"},"thoughtSignature":"signature-a"}]},"finishReason":"MAX_TOKENS"}]}',
    'data: {"usageMetadata":{"candidatesTokenCount":8,"thoughtsTokenCount":2}}',
  ]));
  assertEquals(result, { status: "completed" });
  assertEquals(events, [
    { type: "tool-call-start", index: 0, callId: "call-a", name: "lookup" },
    { type: "tool-call-arguments-delta", index: 0, delta: "{}" },
    { type: "tool-call-end", index: 0 },
    {
      type: "usage",
      usage: {
        inputTokens: null,
        outputTokens: 8,
        totalTokens: null,
        cacheReadTokens: null,
        cacheWriteTokens: null,
        reasoningTokens: 2,
      },
    },
    { type: "done", reason: "length" },
  ]);
});

for (const [scenario, trailer, message] of [
  ["SSE error", ['data: {"response":{"error":{"message":"generate failed"}}}'], "generate failed"],
  ["blocked prompt", ['data: {"promptFeedback":{"blockReason":"SAFETY"}}'], "SAFETY"],
  ["failed finish", ['data: {"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}'],
    "MALFORMED_FUNCTION_CALL"],
  ["EOF", [], "without finishReason"],
  ["early DONE marker", ["data: [DONE]"], "without finishReason"],
] as const) {
  Deno.test(`provider reports ${scenario} without completing the stream`, async () => {
    const { events, result, streamCalls } = await invokeMock(sse([
      'data: {"candidates":[{"content":{"parts":[{"text":"partial"}]},"finishReason":"FINISH_REASON_UNSPECIFIED"}],"usageMetadata":{"promptTokenCount":3}}',
      ...trailer,
    ]));
    assert(result.status === "request-error");
    assert(result.message.includes(message));
    assertEquals(streamCalls, 1);
    assertEquals(events.map((event) => event.type), [
      "text-start",
      "text-delta",
      "text-end",
      "usage",
    ]);
  });
}

Deno.test("provider does not complete a truncated tool call stream", async () => {
  async function* interrupted(): AsyncGenerator<string> {
    yield 'data: {"candidates":[{"content":{"parts":[{"functionCall":{"id":"call-a","name":"lookup"},"thoughtSignature":"signature-a"}]}}]}';
    throw new Error("stream connection closed");
  }
  const { events, result, streamCalls } = await invokeMock(interrupted());
  assertEquals(result, { status: "request-error", message: "stream connection closed" });
  assertEquals(streamCalls, 1);
  assertEquals(events.map((event) => event.type), [
    "tool-call-start",
    "tool-call-arguments-delta",
    "tool-call-end",
  ]);
});

Deno.test("provider keeps endpoint fallback before emitting content or usage", async () => {
  let attempts = 0;
  const lines = {
    [Symbol.asyncIterator]() {
      attempts++;
      return sse(attempts === 1
        ? ['data: {"usageMetadata":{"promptTokenCount":3},"error":{"message":"unavailable"}}']
        : ['data: {"candidates":[{"finishReason":"STOP"}]}']);
    },
  };
  const { events, result, streamCalls } = await invokeMock(lines);
  assertEquals(result, { status: "completed" });
  assertEquals(streamCalls, 2);
  assertEquals(events, [{ type: "done", reason: "stop" }]);
});

Deno.test("isQuotaError identifies rate limits and quota exhaustion", () => {
  assert(isQuotaError("RESOURCE_EXHAUSTED: quota exceeded"));
  assert(isQuotaError("Rate limit exceeded for model"));
  assert(isQuotaError("HTTP 429 Too Many Requests"));
  assert(!isQuotaError("Invalid authorization header"));
});

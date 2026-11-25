import type {
  LlmMessage,
  ModelUsage,
  ProviderInvokeInput,
  ProviderOutput,
  ProviderResult,
  ProviderSupport,
} from "cursor-byok:provider";
import type { JsonValue, PluginContext } from "cursor-byok:plugin";
import { HttpError } from "cursor-byok:protocol/openai-chat";
import {
  ANTIGRAVITY_CLIENT_HEADERS,
  ANTIGRAVITY_ENDPOINTS,
  ANTIGRAVITY_USER_AGENT,
  antigravityModels,
} from "./models.ts";
import {
  type AccountData,
  accountData,
  isTokenExpired,
  quotaExhaustedPatch,
  refreshAccount,
  RESOURCE_TYPE,
} from "./resources.ts";

export function isQuotaError(error: string): boolean {
  const message = error.toLowerCase();
  return message.includes("resource_exhausted") ||
    message.includes("quota_exceeded") ||
    message.includes("quota_exhausted") ||
    message.includes("rate_limit_exceeded") ||
    message.includes("rate limit") ||
    message.includes("model_capacity_exhausted") ||
    message.includes("too many requests") ||
    message.includes("429");
}

function isQuotaHttpError(error: HttpError): boolean {
  if (error.status === 429) return true;
  const body = error.body.toLowerCase();
  return body.includes("resource_exhausted") ||
    body.includes("quota_exceeded") ||
    body.includes("quota_exhausted") ||
    body.includes("rate_limit_exceeded") ||
    body.includes("rate limit") ||
    body.includes("model_capacity_exhausted") ||
    body.includes("user rate limit exceeded") ||
    body.includes("too many requests");
}

function invalidResult(message: string, stateMessage: string): ProviderResult {
  return {
    status: "resource-error",
    message,
    patch: { state: { status: "invalid", message: stateMessage } },
  };
}

async function readBody(lines: AsyncIterable<string>): Promise<string> {
  const collected: string[] = [];
  for await (const line of lines) collected.push(line);
  return collected.join("\n");
}

function resolveAntigravityModel(modelId: string): string {
  const raw = modelId.trim();
  const lower = raw.toLowerCase();

  // 1. If explicit tier is already specified in the model ID, pass it directly!
  if (
    lower.startsWith("gemini-3.8-flash-") ||
    lower.startsWith("gemini-3.7-flash-") ||
    lower.startsWith("gemini-3.6-flash-") ||
    lower.startsWith("gemini-3.1-pro-") ||
    lower === "gemini-3.8-flash" ||
    lower === "gemini-3.7-flash" ||
    lower === "gemini-3.6-flash" ||
    lower === "gemini-2.5-flash" ||
    lower === "gemini-2.5-pro" ||
    lower === "gemini-2.0-flash" ||
    lower === "claude-sonnet-4-6" ||
    lower === "claude-sonnet-4-6-thinking" ||
    lower === "claude-opus-4-6-thinking" ||
    lower === "gemini-3.1-flash-image" ||
    lower === "gpt-oss-120b-medium"
  ) {
    if (lower === "gemini-3.1-pro-high") return "gemini-pro-agent";
    return raw;
  }

  // 2. Canonical Antigravity-Manager mapping for aliases
  if (
    lower === "claude-3-7-sonnet" || lower === "claude-3-5-sonnet" || lower === "claude-sonnet-4-5"
  ) {
    return "claude-sonnet-4-6";
  }
  if (lower === "claude-3-5-haiku" || lower === "claude-haiku-4") {
    return "claude-sonnet-4-6";
  }
  if (
    lower === "claude-3-7-opus" || lower === "claude-opus-4" || lower === "claude-opus-4.6" ||
    lower === "claude-opus-4-5-thinking"
  ) {
    return "claude-opus-4-6-thinking";
  }
  if (
    lower === "gpt-4" || lower === "gpt-4o" || lower === "gpt-4o-mini" || lower === "gpt-3.5-turbo"
  ) {
    return "gemini-2.5-flash";
  }
  if (lower === "gemini-2.5-flash-lite") {
    return "gemini-2.5-flash";
  }
  if (lower === "gemini-3-flash" || lower === "gemini-3.5-flash") {
    return "gemini-3.7-flash";
  }
  if (lower === "gemini-3-pro" || lower === "gemini-3.1-pro") {
    return "gemini-3.1-pro-preview";
  }
  if (lower === "gemini-3-pro-high") {
    return "gemini-pro-agent";
  }

  return raw;
}

function randomHex(length = 8): string {
  const array = new Uint8Array(Math.ceil(length / 2));
  crypto.getRandomValues(array);
  return Array.from(array, (byte) => byte.toString(16).padStart(2, "0")).join("").slice(0, length);
}

function generateRequestId(): string {
  return `agent/${Date.now()}/${randomHex(8)}`;
}

// Keys the CloudCode v1internal Schema proto rejects with "Cannot find field"
const UNSUPPORTED_SCHEMA_KEYS: Record<string, true> = {
  "$schema": true,
  "$ref": true,
  "$defs": true,
  "$comment": true,
  "examples": true,
  "unevaluatedProperties": true,
  "unevaluatedItems": true,
  "patternProperties": true,
  "propertyNames": true,
  "exclusiveMinimum": true,
  "exclusiveMaximum": true,
  "multipleOf": true,
  "dependencies": true,
  "dependentSchemas": true,
  "dependentRequired": true,
  "deprecated": true,
  "readOnly": true,
  "writeOnly": true,
  "x-mcp-header": true,
  "const": true,
  "default": true,
  "additionalProperties": true,
  "title": true,
  "format": true,
};

const PROTO_TYPE_MAP: Record<string, string> = {
  string: "STRING",
  number: "NUMBER",
  integer: "INTEGER",
  boolean: "BOOLEAN",
  array: "ARRAY",
  object: "OBJECT",
};

function enforceUppercaseTypes(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(enforceUppercaseTypes);
  if (value === null || typeof value !== "object") return value;
  const out: Record<string, unknown> = {};
  for (const [key, child] of Object.entries(value as Record<string, unknown>)) {
    if (UNSUPPORTED_SCHEMA_KEYS[key]) continue;
    if (key === "type" && typeof child === "string") {
      out[key] = PROTO_TYPE_MAP[child.toLowerCase()] ?? child.toUpperCase();
    } else {
      out[key] = enforceUppercaseTypes(child);
    }
  }
  if (!out.type && out.properties) {
    out.type = "OBJECT";
  }
  return out;
}

function sanitizeSchema(value: unknown): unknown {
  if (!value || typeof value !== "object") {
    return { type: "OBJECT", properties: {} };
  }
  const clean = enforceUppercaseTypes(value) as Record<string, unknown>;
  if (!clean.type) clean.type = "OBJECT";
  return clean;
}

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 ? value : null;
}

function tokenCount(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function mergeUsage(usage: Record<string, unknown>, previous: ModelUsage | null): ModelUsage {
  return {
    inputTokens: tokenCount(usage.promptTokenCount) ?? previous?.inputTokens ?? null,
    outputTokens: tokenCount(usage.candidatesTokenCount) ?? previous?.outputTokens ?? null,
    totalTokens: tokenCount(usage.totalTokenCount) ?? previous?.totalTokens ?? null,
    cacheReadTokens: tokenCount(usage.cachedContentTokenCount) ?? previous?.cacheReadTokens ?? null,
    cacheWriteTokens: null,
    reasoningTokens: tokenCount(usage.thoughtsTokenCount) ?? previous?.reasoningTokens ?? null,
  };
}

function convertToCloudCodeContents(
  instructions: string,
  messages: LlmMessage[],
): {
  contents: Array<{ role: string; parts: Array<Record<string, unknown>> }>;
  systemInstruction?: { role: string; parts: Array<{ text: string }> };
} {
  const rawContents: Array<{ role: string; parts: Array<Record<string, unknown>> }> = [];
  let systemText = instructions || "";
  let callOrder = new Map<string, number>();

  for (let messageIndex = 0; messageIndex < messages.length; messageIndex++) {
    const msg = messages[messageIndex];
    if (msg.role === "system") {
      const txt = msg.content
        .map((p) => (p.type === "text" ? p.text : ""))
        .filter(Boolean)
        .join("\n");
      if (txt) {
        systemText += (systemText ? "\n\n" : "") + txt;
      }
      continue;
    }

    if (msg.role === "assistant") {
      const parts: Array<Record<string, unknown>> = [];
      if (msg.text) {
        parts.push({ text: msg.text });
      }

      const replayVal = msg.replayState?.providerKind === "antigravity"
        ? record(msg.replayState.value)
        : null;
      const toolSignatures = record(replayVal?.toolCallSignatures);
      const legacySignature = text(replayVal?.thoughtSignature) ?? "skip_thought_signature_validator";

      callOrder = new Map(msg.toolCalls.map((call, index) => [call.callId, index]));
      for (const call of msg.toolCalls) {
        const signature = toolSignatures ? text(toolSignatures[call.callId]) : legacySignature;
        parts.push({
          functionCall: {
            ...(call.callId ? { id: call.callId } : {}),
            name: call.name,
            args: typeof call.arguments === "object" && call.arguments !== null
              ? call.arguments
              : {},
          },
          ...(signature ? { thoughtSignature: signature } : {}),
        });
      }

      if (parts.length > 0) {
        rawContents.push({ role: "model", parts });
      }
    } else if (msg.role === "tool") {
      const results = [msg];
      while (true) {
        const next = messages[messageIndex + 1];
        if (next?.role !== "tool") break;
        results.push(next);
        messageIndex++;
      }
      const resultOrder = (id: string) => callOrder.get(id) ?? Number.MAX_SAFE_INTEGER;
      results.sort((left, right) => resultOrder(left.callId) - resultOrder(right.callId));
      rawContents.push({
        role: "user",
        parts: results.map((result) => ({
          functionResponse: {
            ...(result.callId ? { id: result.callId } : {}),
            name: result.name || "function",
            response: { result: result.content },
          },
        })),
      });
    } else if (msg.role === "user") {
      callOrder.clear();
      const parts: Array<Record<string, unknown>> = [];
      for (const p of msg.content) {
        if (p.type === "text") {
          if (p.text) parts.push({ text: p.text });
        } else if (p.type === "image") {
          parts.push({
            inlineData: {
              mimeType: p.mediaType,
              data: p.dataBase64,
            },
          });
        }
      }
      if (parts.length === 0) {
        parts.push({ text: " " });
      }
      rawContents.push({ role: "user", parts });
    }
  }

  // Merge consecutive same-role messages so contents strictly alternate user -> model -> user -> model
  const contents: Array<{ role: string; parts: Array<Record<string, unknown>> }> = [];
  for (const item of rawContents) {
    if (item.parts.length === 0) continue;
    const last = contents[contents.length - 1];
    if (last && last.role === item.role) {
      last.parts.push(...item.parts);
    } else {
      contents.push(item);
    }
  }

  if (contents.length > 0 && contents[0].role !== "user") {
    contents.unshift({ role: "user", parts: [{ text: " " }] });
  }

  return {
    contents,
    ...(systemText.trim()
      ? { systemInstruction: { role: "system", parts: [{ text: systemText.trim() }] } }
      : {}),
  };
}

async function streamCloudCode(
  accessToken: string,
  projectId: string,
  modelId: string,
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<void> {
  const actualModel = resolveAntigravityModel(modelId);
  const { contents, systemInstruction } = convertToCloudCodeContents(
    input.request.instructions,
    input.request.messages,
  );

  const tools = input.request.tools && input.request.tools.length > 0
    ? [
      {
        functionDeclarations: input.request.tools.map((t) => ({
          name: t.name,
          description: t.description || "",
          parameters: sanitizeSchema(t.parameters),
        })),
      },
    ]
    : undefined;

  const toolConfig = tools
    ? {
      functionCallingConfig: { mode: "AUTO" },
    }
    : undefined;

  const payload = {
    project: projectId || "bamboo-precept-lgxtn",
    model: actualModel,
    userAgent: "antigravity",
    requestType: "agent",
    requestId: generateRequestId(),
    enabledCreditTypes: ["GOOGLE_ONE_AI"],
    request: {
      contents,
      ...(systemInstruction ? { systemInstruction } : {}),
      ...(tools ? { tools } : {}),
      ...(toolConfig ? { toolConfig } : {}),
      generationConfig: {
        maxOutputTokens: 65536,
      },
    },
  };

  const headers: Record<string, string> = {
    authorization: `Bearer ${accessToken}`,
    "content-type": "application/json",
    accept: "text/event-stream",
    "user-agent": ANTIGRAVITY_USER_AGENT,
    ...ANTIGRAVITY_CLIENT_HEADERS,
  };
  if (actualModel.toLowerCase().includes("claude")) {
    headers["anthropic-beta"] =
      "claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
  }

  let lastError: Error | null = null;
  let hasEmittedAnyChunk = false;

  for (const endpoint of ANTIGRAVITY_ENDPOINTS) {
    if (hasEmittedAnyChunk) break;

    try {
      const response = await context.network.stream(
        `${endpoint}/v1internal:streamGenerateContent?alt=sse`,
        {
          method: "POST",
          headers,
          body: JSON.stringify(payload),
        },
      );

      if (response.status < 200 || response.status >= 300) {
        const errorBody = await readBody(response.lines);
        lastError = new HttpError(response.status, errorBody);
        if (
          response.status === 503 || response.status === 502 || response.status === 504 ||
          response.status === 404
        ) {
          continue;
        }
        throw lastError;
      }

      let textStarted = false;
      let thinkingStarted = false;
      let toolIndex = 0;
      let lastThoughtSignature: string | null = null;
      const toolCallSignatures = new Map<string, string>();
      let finalUsage: ModelUsage | null = null;
      let finishReason: string | null = null;
      let doneReason: "stop" | "length" | "tool-use" | null = null;

      const closeThinking = () => {
        if (!thinkingStarted) return;
        thinkingStarted = false;
        output.emit({ type: "thinking-end" });
      };
      const closeText = () => {
        if (!textStarted) return;
        textStarted = false;
        output.emit({ type: "text-end" });
      };

      try {
        for await (const line of response.lines) {
          if (!line.startsWith("data:")) continue;
          const raw = line.slice(5).trim();
          if (!raw) continue;
          if (raw === "[DONE]") break;

          let json: Record<string, unknown> | null;
          try {
            json = record(JSON.parse(raw));
          } catch {
            throw new Error("Cloud Code SSE returned invalid JSON");
          }
          if (!json) throw new Error("Cloud Code SSE returned an invalid event");
          const resp = record(json.response) ?? json;
          const usage = record(resp.usageMetadata);
          if (usage) finalUsage = mergeUsage(usage, finalUsage);

          const error = json.error ?? resp.error;
          if (error !== null && error !== undefined) {
            const details = record(error);
            const message = text(error) ?? text(details?.message) ?? text(details?.status);
            throw new Error(
              message ? `Cloud Code stream error: ${message}` : "Cloud Code stream error",
            );
          }
          const blockReason = text(record(resp.promptFeedback)?.blockReason);
          if (blockReason && blockReason !== "BLOCK_REASON_UNSPECIFIED") {
            throw new Error(`Cloud Code blocked the prompt: ${blockReason}`);
          }
          if (finishReason) continue;

          const candidate = Array.isArray(resp.candidates) ? record(resp.candidates[0]) : null;
          const parts = record(candidate?.content)?.parts;
          if (Array.isArray(parts)) {
            for (const rawPart of parts) {
              const part = record(rawPart);
              if (!part) continue;
              const signature = text(part.thoughtSignature);
              if (signature) lastThoughtSignature = signature;

              const textPart = text(part.text);
              if (part.thought === true && textPart) {
                const cleanThought = textPart.replace(/<\/?think>/gi, "");
                if (cleanThought) {
                  hasEmittedAnyChunk = true;
                  if (!thinkingStarted) {
                    thinkingStarted = true;
                    output.emit({ type: "thinking-start" });
                  }
                  output.emit({ type: "thinking-delta", text: cleanThought });
                }
              } else if (textPart) {
                closeThinking();
                const cleanText = textPart.replace(/<\/?think>/gi, "");
                if (cleanText) {
                  hasEmittedAnyChunk = true;
                  if (!textStarted) {
                    textStarted = true;
                    output.emit({ type: "text-start" });
                  }
                  output.emit({ type: "text-delta", text: cleanText });
                }
              }

              const fnCall = record(part.functionCall);
              if (fnCall) {
                const name = text(fnCall.name);
                if (!name) throw new Error("Cloud Code tool call is missing name");
                hasEmittedAnyChunk = true;
                closeThinking();
                closeText();
                const index = toolIndex++;
                const callId = text(fnCall.id) ?? `call_${crypto.randomUUID()}`;
                if (signature) toolCallSignatures.set(callId, signature);
                output.emit({ type: "tool-call-start", index, callId, name });
                const delta = typeof fnCall.args === "string"
                  ? fnCall.args
                  : JSON.stringify(fnCall.args || {});
                output.emit({ type: "tool-call-arguments-delta", index, delta });
                output.emit({ type: "tool-call-end", index });
              }
            }
          }

          const reason = text(candidate?.finishReason);
          if (reason !== "FINISH_REASON_UNSPECIFIED") finishReason = reason;
        }

        if (!finishReason) throw new Error("Cloud Code stream ended without finishReason");
        if (finishReason !== "STOP" && finishReason !== "MAX_TOKENS") {
          throw new Error(`Cloud Code generation ended with ${finishReason}`);
        }
        doneReason = finishReason === "MAX_TOKENS" ? "length" : toolIndex > 0 ? "tool-use" : "stop";
      } finally {
        closeThinking();
        closeText();
        if (doneReason === "stop" || doneReason === "tool-use") {
          const value: JsonValue = toolCallSignatures.size > 0
            ? { toolCallSignatures: Object.fromEntries(toolCallSignatures) }
            : lastThoughtSignature
            ? { thoughtSignature: lastThoughtSignature }
            : null;
          if (value) output.emit({ type: "replay-state", providerKind: "antigravity", value });
        }
        if (finalUsage && (doneReason || hasEmittedAnyChunk)) {
          output.emit({ type: "usage", usage: finalUsage });
        }
      }
      output.emit({ type: "done", reason: doneReason });
      return;
    } catch (err) {
      lastError = err instanceof Error ? err : new Error(String(err));
      if (hasEmittedAnyChunk) {
        throw lastError;
      }
    }
  }

  if (lastError) throw lastError;
}

async function invoke(
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<ProviderResult> {
  if (!input.resource) {
    return {
      status: "request-error",
      message: "Add a Google Antigravity account or API key before calling Antigravity",
    };
  }
  let data: AccountData;
  try {
    data = accountData(input.resource);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return invalidResult(message, message);
  }

  let patchData: AccountData | null = null;

  // Auto-refresh token if expired or close to expiration (skew 5 mins)
  if (data.refreshToken && isTokenExpired(data)) {
    try {
      const refreshed = await refreshAccount(input.resource, context);
      if (refreshed.privateData) {
        data = refreshed.privateData as unknown as AccountData;
        patchData = data;
      }
    } catch {
      // Continue with existing token
    }
  }

  let projectId = data.projectId ?? "bamboo-precept-lgxtn";

  try {
    await streamCloudCode(data.accessToken, projectId, input.model.id, input, output, context);
    return patchData
      ? {
        status: "completed",
        patch: { privateData: patchData as unknown as JsonValue, state: { status: "ready" } },
      }
      : { status: "completed" };
  } catch (error) {
    if (error instanceof HttpError) {
      if (
        (error.status === 401 || error.status === 403) && data.refreshToken &&
        !isQuotaHttpError(error)
      ) {
        try {
          const refreshed = await refreshAccount(input.resource, context);
          if (refreshed.privateData) {
            const freshData = refreshed.privateData as unknown as AccountData;
            const freshProj = freshData.projectId ?? projectId;
            await streamCloudCode(
              freshData.accessToken,
              freshProj,
              input.model.id,
              input,
              output,
              context,
            );
            return {
              status: "completed",
              patch: { privateData: freshData as unknown as JsonValue, state: { status: "ready" } },
            };
          }
        } catch {
          // Failed refresh
        }
      }
      if (isQuotaHttpError(error)) {
        return {
          status: "resource-error",
          message: error.message,
          patch: quotaExhaustedPatch(data, error.body),
        };
      }
      return { status: "request-error", message: error.message };
    }
    const message = error instanceof Error ? error.message : String(error);
    if (isQuotaError(message)) {
      return { status: "resource-error", message, patch: quotaExhaustedPatch(data, message) };
    }
    return { status: "request-error", message };
  }
}

export const antigravityProvider: ProviderSupport = {
  id: "antigravity",
  displayName: {
    "en-US": "Google Antigravity",
    "zh-CN": "Google Antigravity",
  },
  description: {
    "en-US": "Google Antigravity / Gemini model access with hybrid reasoning & agent tools.",
    "zh-CN": "通过 Google Antigravity / Gemini API 使用混合推理与 Agent 工具。",
  },
  providerType: "google",
  resourceType: RESOURCE_TYPE,
  models: antigravityModels,
  invoke,
};

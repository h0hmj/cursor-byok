import type {
  JsonValue,
  NetworkEventStream,
  NetworkResponse,
  PluginContext,
} from "cursor-byok:plugin";
import type { LlmRequest, ModelEvent } from "cursor-byok:provider";
import type { ResourcePatch, ResourceSnapshot } from "cursor-byok:resource";
import { codexDeviceOAuth } from "./oauth.ts";
import { codexModels, parseOfficialModels } from "./models.ts";
import { buildResponsesBody, HttpError } from "cursor-byok:protocol/openai-responses";
import { codexProvider, isQuotaError } from "./provider.ts";
import {
  accountIdentity,
  consumeResetCardAction,
  credentialDraft,
  listResetCardsAction,
  parseCodexUsage,
  parseCredentialFiles,
  presentAccount,
  quotaState,
  refreshAccount,
  RESOURCE_TYPE,
  tokenExpiresAtMs,
} from "./resources.ts";
import { withAccountAuth } from "./auth.ts";

function assert(condition: unknown, message = "assertion failed"): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

function jwt(payload: Record<string, unknown>): string {
  const encoded = btoa(JSON.stringify(payload)).replace(/=/g, "").replace(/\+/g, "-").replace(
    /\//g,
    "_",
  );
  return `header.${encoded}.signature`;
}

type RequestInit = { body?: string; headers?: Record<string, string>; sensitive?: boolean };
type FetchHandler = (url: string, init?: RequestInit) => NetworkResponse | Promise<NetworkResponse>;
type StreamHandler = (url: string, init?: RequestInit) => NetworkEventStream;

function context(handlers: {
  fetch?: FetchHandler;
  stream?: StreamHandler;
  resource?: ResourceSnapshot;
  patches?: ResourcePatch[];
}): PluginContext {
  let resource = handlers.resource ?? snapshot({
    accessToken: "access-secret",
    refreshToken: null,
    accountId: "acct-1",
    displayName: "person@example.com",
    quota: null,
  });
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
    resource: {
      read: () => Promise.resolve(resource),
      patch: (patch) => {
        handlers.patches?.push(patch);
        resource = {
          ...resource,
          ...(patch.privateData === undefined ? {} : { privateData: patch.privateData }),
          ...(patch.privateDataFields === undefined ? {} : {
            privateData: {
              ...resource.privateData as Record<string, JsonValue>,
              ...patch.privateDataFields,
            },
          }),
          ...(patch.state === undefined ? {} : { state: patch.state }),
        };
        return Promise.resolve(resource);
      },
    },
    signal: new AbortController().signal,
  };
}

function snapshot(privateData: JsonValue): ResourceSnapshot {
  return {
    id: "resource-1",
    type: RESOURCE_TYPE,
    key: "codex:acct-1",
    privateData,
    state: { status: "ready" },
  };
}

async function* sse(lines: string[]): AsyncGenerator<string> {
  for (const line of lines) yield line;
}

function request(): LlmRequest {
  return {
    instructions: "You are a coding assistant.",
    messages: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
    tools: [],
    reasoning: { enabled: true, effort: "medium" },
    latency: "fast",
    maxOutputTokens: 128_000,
    cacheKey: "conversation-1",
  };
}

Deno.test("account identity prioritizes ChatGPT account ID and drafts keep tokens private-side", async () => {
  const token = jwt({
    "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" },
    sub: "subject-1",
    email: "person@example.com",
  });
  assertEquals(await accountIdentity(token), {
    key: "codex:acct-1",
    displayName: "person@example.com",
  });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  assertEquals(draft.key, "codex:acct-1");
  const view = presentAccount(snapshot(draft.privateData));
  assert(!JSON.stringify(view).includes(token), "resource view exposed an access token");
  assertEquals(view.displayName, "person@example.com");
});

Deno.test("credential import accepts Codex auth JSON files", () => {
  const { credentials, warnings } = parseCredentialFiles([
    {
      name: "auth.json",
      content: JSON.stringify({
        tokens: {
          access_token: "access-secret",
          refresh_token: "refresh-secret",
          id_token: jwt({ email: "person@example.com" }),
        },
      }),
    },
    { name: "broken.json", content: "{not json" },
  ]);
  assertEquals(credentials, [{
    accessToken: "access-secret",
    refreshToken: "refresh-secret",
    displayName: "person@example.com",
  }]);
  assertEquals(warnings, ["broken.json: not valid JSON"]);
});

Deno.test("usage maps secondary to weekly and primary to five-hour quota", () => {
  const quota = parseCodexUsage({
    plan_type: "plus",
    rate_limit: {
      primary_window: { used_percent: 80, reset_at: 1_800_000_000 },
      secondary_window: { used_percent: 25, reset_at: 1_900_000_000 },
    },
    rate_limit_reset_credits: { available_count: 2 },
  }, 1_700_000_000_000);
  assertEquals(quota.planLabel, "ChatGPT Plus");
  assertEquals(quota.weekly?.remainingPercent, 75);
  assertEquals(quota.fiveHour?.remainingPercent, 20);
  assertEquals(quota.weekly?.resetAtMs, 1_900_000_000_000);
  assertEquals(quota.resetCreditsAvailable, 2);
  assertEquals(quotaState(quota, 1_700_000_000_000), { status: "ready" });
});

Deno.test("reset card action lists safe card metadata and optional expiry", async () => {
  const result = await listResetCardsAction.run(
    snapshot({
      accessToken: "access-secret",
      accountId: "acct-1",
      refreshToken: null,
      displayName: "person@example.com",
      quota: null,
    }),
    null,
    context({
      fetch: (url, init) => {
        assertEquals(url, "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits");
        assertEquals(init?.headers?.["ChatGPT-Account-Id"], "acct-1");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            available_count: 1,
            credits: [{
              id: "credit-1",
              reset_type: "codex_rate_limits",
              status: "available",
              granted_at: "2026-06-12T01:33:14Z",
              expires_at: "2026-07-12T01:33:14Z",
              title: "One free rate limit reset",
            }],
          }),
        };
      },
    }),
  );
  assertEquals(result.cards, [{
    id: "credit-1",
    title: "One free rate limit reset",
    status: "available",
    grantedAtMs: Date.parse("2026-06-12T01:33:14Z"),
    expiresAtMs: Date.parse("2026-07-12T01:33:14Z"),
    fields: [{
      id: "reset-type",
      label: { "en-US": "Reset type", "zh-CN": "重置类型" },
      value: "codex_rate_limits",
    }],
  }]);
});

Deno.test("reset card action consumes a selected card and refreshes quota state", async () => {
  let requestNumber = 0;
  const result = await consumeResetCardAction.run(
    snapshot({
      accessToken: "access-secret",
      accountId: "acct-1",
      refreshToken: null,
      displayName: "person@example.com",
      quota: null,
    }),
    { cardId: "credit-1" },
    context({
      fetch: (url, init) => {
        requestNumber += 1;
        if (requestNumber === 1 || requestNumber === 4) {
          assertEquals(url, "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits");
          return {
            status: 200,
            headers: {},
            body: JSON.stringify(
              requestNumber === 1
                ? { available_count: 1, credits: [{ id: "credit-1", status: "available" }] }
                : { available_count: 0, credits: [] },
            ),
          };
        }
        if (requestNumber === 2) {
          assertEquals(
            url,
            "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume",
          );
          assertEquals(JSON.parse(init?.body ?? "{}"), {
            credit_id: "credit-1",
            redeem_request_id: JSON.parse(init?.body ?? "{}").redeem_request_id,
          });
          return { status: 200, headers: {}, body: JSON.stringify({ code: "reset" }) };
        }
        assertEquals(url, "https://chatgpt.com/backend-api/wham/usage");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({ rate_limit_reset_credits: { available_count: 0 } }),
        };
      },
    }),
  );
  assertEquals(requestNumber, 4);
  assertEquals(result.cards, []);
  const quota = result.patch?.privateDataFields?.quota as Record<
    string,
    unknown
  >;
  assertEquals(quota.resetCreditsAvailable, 0);
  assertEquals(quota.weekly, null);
  assertEquals(quota.fiveHour, null);
});

Deno.test("exhausted quota projects a cooling state until the latest reset", () => {
  const quota = parseCodexUsage({
    rate_limit: {
      primary_window: { used_percent: 100, reset_at: 1_800_000_000 },
      secondary_window: { used_percent: 100, reset_at: 1_900_000_000 },
    },
  }, 1_700_000_000_000);
  assertEquals(quotaState(quota, 1_700_000_000_000), {
    status: "cooling",
    retryAtMs: 1_900_000_000_000,
    message: "ChatGPT quota is exhausted",
  });
});

Deno.test("official model discovery excludes hidden models and puts the default first", () => {
  const models = parseOfficialModels({
    default_model: "gpt-second",
    models: [
      {
        slug: "gpt-first",
        display_name: "GPT First",
        supported_in_api: true,
        visibility: "list",
        supported_reasoning_levels: [
          { effort: "low", description: "Fast responses" },
          { effort: "medium", description: "Balanced" },
        ],
      },
      { slug: "gpt-second", supported_in_api: true, visibility: "list" },
      { slug: "gpt-hidden", supported_in_api: true, visibility: "hidden" },
      { slug: "gpt-internal", supported_in_api: false, visibility: "list" },
    ],
  });
  assertEquals(models.map((model) => model.id), ["gpt-second", "gpt-first"]);
  assertEquals(models[1].capabilities, { images: true });
  assertEquals(models[1].privateData, { reasoningEfforts: ["low", "medium"] });
});

Deno.test("device OAuth begins with a host-held session and completes with a resource draft", async () => {
  const accessToken = jwt({
    "https://api.openai.com/auth": { chatgpt_account_id: "acct-oauth" },
    email: "oauth@example.com",
  });
  let requestNumber = 0;
  const flowContext = context({
    fetch: (url, init) => {
      requestNumber += 1;
      if (requestNumber === 1) {
        assertEquals(url, "https://auth.openai.com/api/accounts/deviceauth/usercode");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            device_auth_id: "private-device-id",
            user_code: "ABCD-EFGH",
            expires_in: 900,
            interval: 5,
          }),
        };
      }
      if (requestNumber === 2) {
        assertEquals(url, "https://auth.openai.com/api/accounts/deviceauth/token");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            authorization_code: "authorization-code",
            code_verifier: "pkce-verifier",
          }),
        };
      }
      assertEquals(url, "https://auth.openai.com/oauth/token");
      assert(init?.body?.includes("grant_type=authorization_code"));
      assert(init?.body?.includes("code_verifier=pkce-verifier"));
      return {
        status: 200,
        headers: {},
        body: JSON.stringify({ access_token: accessToken, refresh_token: "refresh-secret" }),
      };
    },
  });

  const begun = await codexDeviceOAuth.begin(flowContext);
  assertEquals(begun.userCode, "ABCD-EFGH");
  assertEquals(begun.pollIntervalMs, 5000);

  const polled = await codexDeviceOAuth.poll(begun.session, flowContext);
  assert(polled.status === "completed", `expected completed, received ${polled.status}`);
  assertEquals(polled.resources[0].key, "codex:acct-oauth");
  assertEquals(requestNumber, 3);
});

Deno.test("invoke streams normalized events from the Codex Responses API", async () => {
  const token = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" } });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  let requestBody = "";
  let requestHeaders: Record<string, string> = {};
  const events: ModelEvent[] = [];
  const result = await codexProvider.invoke(
    {
      model: {
        id: "gpt-test",
        displayName: "GPT Test",
        privateData: { reasoningEfforts: ["medium"] },
      },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      resource: snapshot(draft.privateData),
      stream: (url, init) => {
        assertEquals(url, "https://chatgpt.com/backend-api/codex/responses");
        requestBody = init?.body ?? "";
        requestHeaders = init?.headers ?? {};
        return {
          status: 200,
          headers: {},
          lines: sse([
            'data: {"type":"response.output_text.delta","delta":"Hel"}',
            'data: {"type":"response.output_text.delta","delta":"lo"}',
            'data: {"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":2,"input_tokens_details":{"cached_tokens":4}}}}',
          ]),
        };
      },
    }),
  );
  assertEquals(result, { status: "completed" });
  const body = JSON.parse(requestBody) as Record<string, unknown>;
  assertEquals(body.model, "gpt-test");
  assertEquals(body.store, false);
  assertEquals(body.reasoning, { summary: "auto", effort: "medium" });
  assertEquals(body.instructions, "You are a coding assistant.");
  assertEquals(body.include, ["reasoning.encrypted_content"]);
  assert(!("max_output_tokens" in body), "Codex endpoint rejects max_output_tokens");
  assertEquals(body.service_tier, "priority");
  assertEquals(body.prompt_cache_key, "conversation-1");
  // 缓存亲和头与 prompt_cache_key 同源。
  assertEquals(requestHeaders["session-id"], "conversation-1");
  assertEquals(requestHeaders["thread-id"], "conversation-1");
  assertEquals(requestHeaders["x-client-request-id"], "conversation-1");
  assertEquals(events, [
    { type: "text-start" },
    { type: "text-delta", text: "Hel" },
    { type: "text-delta", text: "lo" },
    {
      type: "usage",
      usage: {
        inputTokens: 10,
        outputTokens: 2,
        totalTokens: null,
        cacheReadTokens: 4,
        cacheWriteTokens: null,
        reasoningTokens: null,
      },
    },
    { type: "text-end" },
    { type: "done", reason: "stop" },
  ]);
});

Deno.test("reasoning replay projects response items to valid input items", () => {
  const replayRequest = request();
  replayRequest.messages = [{
    role: "assistant",
    text: "",
    thinking: "",
    replayState: {
      providerKind: "openai_responses",
      value: {
        items: [{
          type: "reasoning",
          id: "item-1",
          status: "completed",
          summary: [{ type: "summary_text", text: "why" }],
          content: [],
          encrypted_content: "opaque",
          output_only: true,
        }],
      },
    },
    toolCalls: [],
  }];

  const body = buildResponsesBody({
    url: "https://example.com/responses",
    model: "gpt-test",
    request: replayRequest,
  });
  assertEquals(body.input, [{
    type: "reasoning",
    id: "item-1",
    summary: [{ type: "summary_text", text: "why" }],
    content: [],
    encrypted_content: "opaque",
  }]);
});

Deno.test("invoke streams incremental tool calls and replays reasoning items", async () => {
  const token = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" } });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const events: ModelEvent[] = [];
  const result = await codexProvider.invoke(
    {
      model: { id: "gpt-test", displayName: "GPT Test" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: (event) => events.push(event) },
    context({
      resource: snapshot(draft.privateData),
      stream: () => ({
        status: 200,
        headers: {},
        lines: sse([
          'data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"read_file"}}',
          'data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\\"path\\":"}',
          'data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"\\"a.ts\\"}"}',
          'data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{\\"path\\":\\"a.ts\\"}"}}',
          'data: {"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","encrypted_content":"opaque"}}',
          'data: {"type":"response.completed","response":{}}',
        ]),
      }),
    }),
  );
  assertEquals(result, { status: "completed" });
  assertEquals(events, [
    { type: "tool-call-start", index: 0, callId: "call-1", name: "read_file" },
    { type: "tool-call-arguments-delta", index: 0, delta: '{"path":' },
    { type: "tool-call-arguments-delta", index: 0, delta: '"a.ts"}' },
    { type: "tool-call-end", index: 0 },
    {
      type: "replay-state",
      providerKind: "openai_responses",
      value: { items: [{ type: "reasoning", encrypted_content: "opaque" }] },
    },
    { type: "done", reason: "tool-use" },
  ]);
});

Deno.test("invoke maps quota failures to a cooling resource error", async () => {
  assert(!isQuotaError("429 rate_limit_reached"));
  assert(isQuotaError("429 usage_limit_reached: 5-hour limit"));
  const token = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" } });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const result = await codexProvider.invoke(
    {
      model: { id: "gpt-test", displayName: "GPT Test" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: () => {} },
    context({
      resource: snapshot(draft.privateData),
      stream: () => ({
        status: 429,
        headers: {},
        lines: sse(['{"detail":"usage_limit_reached","reset_after_seconds":600}']),
      }),
    }),
  );
  assert(result.status === "resource-error", `expected resource-error, received ${result.status}`);
  assert(result.patch.state?.status === "cooling", "quota failure should cool the resource");
  assert(
    result.patch.state.retryAtMs !== undefined && result.patch.state.retryAtMs > Date.now(),
    "cooling should carry the parsed reset time",
  );
});

function account(
  accessToken: string,
  refreshToken: string | null = "refresh-old",
): ResourceSnapshot {
  return snapshot({
    accessToken,
    refreshToken,
    accountId: "acct-1",
    displayName: "person@example.com",
    quota: null,
  });
}

function expiringToken(seconds: number): string {
  return jwt({
    exp: Math.floor(Date.now() / 1000) + seconds,
    "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" },
  });
}

function successStream(): NetworkEventStream {
  return {
    status: 200,
    headers: {},
    lines: sse(['data: {"type":"response.completed","response":{}}']),
  };
}

function invokeAccount(resource: ResourceSnapshot, ctx: PluginContext, events: ModelEvent[] = []) {
  return codexProvider.invoke(
    {
      model: { id: "gpt-test", displayName: "GPT Test" },
      resource,
      request: request(),
    },
    { emit: (event) => events.push(event) },
    ctx,
  );
}

Deno.test("token expiry reads JWT exp without assuming opaque tokens are expired", () => {
  assertEquals(tokenExpiresAtMs(jwt({ exp: 1_800_000_000 })), 1_800_000_000_000);
  assertEquals(tokenExpiresAtMs("opaque"), null);
  assertEquals(tokenExpiresAtMs(jwt({})), null);
  assertEquals(tokenExpiresAtMs("header.invalid.signature"), null);
});

for (const seconds of [-60, 120]) {
  Deno.test(`invoke refreshes tokens with ${seconds}s remaining before opening the stream`, async () => {
    const resource = account(expiringToken(seconds));
    const accessToken = expiringToken(3600);
    const patches: ResourcePatch[] = [];
    let refreshes = 0;
    const ctx = context({
      resource,
      patches,
      fetch: (url, init) => {
        refreshes++;
        assertEquals(url, "https://auth.openai.com/oauth/token");
        assertEquals(init?.sensitive, true);
        const params = new URLSearchParams(init?.body);
        assertEquals(params.get("grant_type"), "refresh_token");
        assertEquals(params.get("client_id"), "app_EMoamEEZ73f0CkXaXp7hrann");
        assertEquals(params.get("refresh_token"), "refresh-old");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({ access_token: accessToken, refresh_token: "refresh-new" }),
        };
      },
      stream: (_url, init) => {
        assertEquals(init?.headers?.authorization, `Bearer ${accessToken}`);
        assert(patches.length === 1, "credentials must be saved before the model request");
        return successStream();
      },
    });
    assertEquals(await invokeAccount(resource, ctx), { status: "completed" });
    assertEquals(refreshes, 1);
    const saved = await ctx.resource!.read();
    assertEquals((saved.privateData as Record<string, unknown>).refreshToken, "refresh-new");
    assertEquals(saved.state, { status: "ready" });
    assert(!JSON.stringify(presentAccount(saved)).includes("refresh-new"));
  });
}

Deno.test("401 refreshes an opaque token and retries with the same body and cache headers", async () => {
  const resource = account("old-access");
  let streams = 0;
  let refreshes = 0;
  const bodies: string[] = [];
  const ctx = context({
    resource,
    fetch: () => {
      refreshes++;
      return { status: 200, headers: {}, body: '{"access_token":"new-access"}' };
    },
    stream: (_url, init) => {
      streams++;
      bodies.push(init?.body ?? "");
      assertEquals(init?.headers?.["session-id"], "conversation-1");
      assertEquals(init?.headers?.["ChatGPT-Account-Id"], "acct-1");
      assertEquals(
        init?.headers?.authorization,
        streams === 1 ? "Bearer old-access" : "Bearer new-access",
      );
      return streams === 1
        ? {
          status: 401,
          headers: {},
          lines: sse(['{"message":"Provided authentication token is expired"}']),
        }
        : successStream();
    },
  });
  assertEquals(await invokeAccount(resource, ctx), { status: "completed" });
  assertEquals(streams, 2);
  assertEquals(refreshes, 1);
  assertEquals(bodies[0], bodies[1]);
  assertEquals((await ctx.resource!.read()).privateData, {
    accessToken: "new-access",
    refreshToken: "refresh-old",
    accountId: "acct-1",
    displayName: "person@example.com",
    quota: null,
  });
});

Deno.test("concurrent expired snapshots share one refresh and read persisted credentials", async () => {
  const resource = account(expiringToken(-60));
  let refreshes = 0;
  let streams = 0;
  const ctx = context({
    resource,
    fetch: async () => {
      refreshes++;
      await Promise.resolve();
      return {
        status: 200,
        headers: {},
        body: JSON.stringify({
          access_token: expiringToken(3600),
          refresh_token: "rotated-refresh",
        }),
      };
    },
    stream: () => {
      streams++;
      return successStream();
    },
  });
  assertEquals(await Promise.all([invokeAccount(resource, ctx), invokeAccount(resource, ctx)]), [
    { status: "completed" },
    { status: "completed" },
  ]);
  assertEquals(refreshes, 1);
  assertEquals(streams, 2);
  assertEquals(await invokeAccount(resource, ctx), { status: "completed" });
  assert(refreshes === 1, "stale snapshots must not reuse rotated refresh tokens");
});

Deno.test("concurrent 401 responses do not refresh the same rejected token twice", async () => {
  const resource = account("old-access");
  let refreshes = 0;
  let rejected = 0;
  const ctx = context({
    resource,
    fetch: () => {
      refreshes++;
      return {
        status: 200,
        headers: {},
        body: '{"access_token":"new-access","refresh_token":"rotated"}',
      };
    },
    stream: (_url, init) => {
      if (init?.headers?.authorization === "Bearer old-access") {
        rejected++;
        return { status: 401, headers: {}, lines: sse(["expired"]) };
      }
      return successStream();
    },
  });
  const results = await Promise.all([invokeAccount(resource, ctx), invokeAccount(resource, ctx)]);
  assertEquals(results, [{ status: "completed" }, { status: "completed" }]);
  assertEquals(rejected, 2);
  assertEquals(refreshes, 1);
});

for (
  const error of [
    "invalid_grant",
    "refresh_token_expired",
    "refresh_token_reused",
    "refresh_token_invalidated",
  ]
) {
  Deno.test(`permanent refresh failure ${error} marks the account invalid without leaking credentials`, async () => {
    const resource = account(expiringToken(-60));
    const ctx = context({
      resource,
      fetch: () => ({
        status: 400,
        headers: {},
        body: JSON.stringify({
          error: { code: error, message: "do not expose refresh-old" },
        }),
      }),
    });
    const result = await invokeAccount(resource, ctx);
    assertEquals(result.status, "resource-error");
    assertEquals((await ctx.resource!.read()).state.status, "invalid");
    assert(!JSON.stringify(result).includes("refresh-old"));
  });
}

for (const status of [429, 500]) {
  Deno.test(`temporary token endpoint HTTP ${status} does not invalidate the account and can be retried`, async () => {
    const resource = account(expiringToken(-60));
    let calls = 0;
    const ctx = context({
      resource,
      fetch: () => {
        calls++;
        return calls === 1 ? { status, headers: {}, body: "private refresh-old" } : {
          status: 200,
          headers: {},
          body: JSON.stringify({ access_token: expiringToken(3600) }),
        };
      },
      stream: () => successStream(),
    });
    const result = await invokeAccount(resource, ctx);
    assertEquals(result.status, "request-error");
    assertEquals((await ctx.resource!.read()).state.status, "ready");
    assert(!JSON.stringify(result).includes("refresh-old"));
    assertEquals(await invokeAccount(resource, ctx), { status: "completed" });
    assertEquals(calls, 2);
  });
}

Deno.test("401 without a refresh token requires sign-in without calling the token endpoint", async () => {
  const resource = account("old-access", null);
  const ctx = context({
    resource,
    stream: () => ({ status: 401, headers: {}, lines: sse(["expired"]) }),
  });
  assertEquals((await invokeAccount(resource, ctx)).status, "resource-error");
  assertEquals((await ctx.resource!.read()).state.status, "invalid");
});

Deno.test("a second 401 stops after one retry and keeps rotated credentials persisted", async () => {
  const resource = account("old-access");
  let refreshes = 0;
  let streams = 0;
  const ctx = context({
    resource,
    fetch: () => {
      refreshes++;
      return {
        status: 200,
        headers: {},
        body: '{"access_token":"new-access","refresh_token":"rotated"}',
      };
    },
    stream: () => {
      streams++;
      return { status: 401, headers: {}, lines: sse(["expired"]) };
    },
  });
  assertEquals((await invokeAccount(resource, ctx)).status, "resource-error");
  assertEquals(refreshes, 1);
  assertEquals(streams, 2);
  assertEquals((await ctx.resource!.read()).state.status, "invalid");
  assertEquals(
    ((await ctx.resource!.read()).privateData as Record<string, unknown>).refreshToken,
    "rotated",
  );
});

Deno.test("model failure after renewal does not lose the new refresh token", async () => {
  const resource = account(expiringToken(-60));
  const ctx = context({
    resource,
    fetch: () => ({
      status: 200,
      headers: {},
      body: '{"access_token":"new-access","refresh_token":"rotated"}',
    }),
    stream: () => ({ status: 503, headers: {}, lines: sse(["unavailable"]) }),
  });
  assertEquals((await invokeAccount(resource, ctx)).status, "request-error");
  const saved = await ctx.resource!.read();
  assertEquals((saved.privateData as Record<string, unknown>).refreshToken, "rotated");
  assertEquals(saved.state.status, "ready");
});

Deno.test("a stream failure after output is never replayed", async () => {
  const resource = account("old-access");
  const events: ModelEvent[] = [];
  let streams = 0;
  const ctx = context({
    resource,
    stream: () => {
      streams++;
      return {
        status: 200,
        headers: {},
        lines: (async function* () {
          yield 'data: {"type":"response.output_text.delta","delta":"partial"}';
          throw new HttpError(401, "expired after output");
        })(),
      };
    },
  });
  assertEquals((await invokeAccount(resource, ctx, events)).status, "resource-error");
  assertEquals(streams, 1);
  assert(events.some((event) => event.type === "text-delta"));
});

Deno.test("manual quota refresh renews and recovers a previously invalid account", async () => {
  const resource = { ...account(expiringToken(-60)), state: { status: "invalid" as const } };
  let calls = 0;
  const ctx = context({
    resource,
    fetch: (url, init) => {
      calls++;
      if (url === "https://auth.openai.com/oauth/token") {
        return {
          status: 200,
          headers: {},
          body: '{"access_token":"new-access","refresh_token":"rotated"}',
        };
      }
      assertEquals(init?.headers?.authorization, "Bearer new-access");
      return { status: 200, headers: {}, body: '{"plan_type":"plus"}' };
    },
  });
  const patch = await refreshAccount(resource, ctx);
  assertEquals(calls, 2);
  assertEquals(patch.state, { status: "ready" });
  assertEquals(
    ((await ctx.resource!.read()).privateData as Record<string, unknown>).refreshToken,
    "rotated",
  );
});

Deno.test("quota lookup 401 renews and retries instead of invalidating a refreshable account", async () => {
  const resource = account("old-access");
  const urls: string[] = [];
  const ctx = context({
    resource,
    fetch: (url, init) => {
      urls.push(url);
      if (url === "https://auth.openai.com/oauth/token") {
        return { status: 200, headers: {}, body: '{"access_token":"new-access"}' };
      }
      return init?.headers?.authorization === "Bearer old-access"
        ? { status: 401, headers: {}, body: "expired" }
        : { status: 200, headers: {}, body: "{}" };
    },
  });
  assertEquals((await refreshAccount(resource, ctx)).state, { status: "ready" });
  assertEquals(urls.length, 3);
});

Deno.test("model discovery renews and persists tokens before syncing models", async () => {
  const resource = account(expiringToken(-60));
  const ctx = context({
    resource,
    fetch: (url, init) => {
      if (url === "https://auth.openai.com/oauth/token") {
        return {
          status: 200,
          headers: {},
          body: '{"access_token":"new-access","refresh_token":"rotated"}',
        };
      }
      assertEquals(init?.headers?.authorization, "Bearer new-access");
      return { status: 200, headers: {}, body: '{"models":[{"slug":"gpt-test"}]}' };
    },
  });
  const models = await codexModels.list({ resource }, ctx);
  assertEquals(models.map((model) => model.id), ["gpt-test"]);
  assertEquals(
    ((await ctx.resource!.read()).privateData as Record<string, unknown>).refreshToken,
    "rotated",
  );
});

Deno.test("reset-card lookups recover from 401 without replaying card consumption", async () => {
  const resource = account("old-access");
  let consumption = 0;
  let lookups = 0;
  const ctx = context({
    resource,
    fetch: (url, init) => {
      if (url === "https://auth.openai.com/oauth/token") {
        return { status: 200, headers: {}, body: '{"access_token":"new-access"}' };
      }
      if (url.endsWith("/consume")) {
        consumption++;
        return { status: 200, headers: {}, body: "{}" };
      }
      if (url.endsWith("/usage")) return { status: 200, headers: {}, body: "{}" };
      lookups++;
      if (lookups === 2 && init?.headers?.authorization === "Bearer old-access") {
        return { status: 401, headers: {}, body: "expired" };
      }
      return {
        status: 200,
        headers: {},
        body: JSON.stringify(
          consumption
            ? { credits: [], available_count: 0 }
            : { credits: [{ id: "card-1", status: "available" }], available_count: 1 },
        ),
      };
    },
  });
  const result = await consumeResetCardAction.run(resource, { cardId: "card-1" }, ctx);
  assertEquals(result.cards, []);
  assertEquals(consumption, 1);
  assertEquals(lookups, 3);
});

Deno.test("refresh cancellation releases the account queue for the next request", async () => {
  const resource = account(expiringToken(-60));
  const ctx = context({
    resource,
    fetch: () => ({ status: 200, headers: {}, body: '{"access_token":"new-access"}' }),
  });
  const controller = new AbortController();
  controller.abort();
  let cancelled = false;
  try {
    await withAccountAuth(resource, { ...ctx, signal: controller.signal }, () => Promise.resolve());
  } catch {
    cancelled = true;
  }
  assert(cancelled);
  assertEquals(
    await withAccountAuth(resource, ctx, (data) => Promise.resolve(data.accessToken)),
    "new-access",
  );
});

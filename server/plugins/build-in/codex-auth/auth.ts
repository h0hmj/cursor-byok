import type { PluginContext } from "cursor-byok:plugin";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { HttpError } from "cursor-byok:protocol/openai-responses";
import {
  type AccountData,
  accountData,
  chatGptAccountId,
  quotaState,
  tokenExpiresAtMs,
} from "./resources.ts";

export const CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann";
export const OAUTH_TOKEN_URL = "https://auth.openai.com/oauth/token";
const REFRESH_MARGIN_MS = 5 * 60 * 1000;
const AUTHORIZATION_EXPIRED = "ChatGPT authorization expired or revoked; sign in again";

export class CodexAuthorizationError extends Error {
  constructor() {
    super(AUTHORIZATION_EXPIRED);
  }
}

// 只协调正在执行的续期,凭证始终由宿主持有。锁内重新读取,避免并发使用已轮换的 refresh token。
const refreshQueues = new Map<string, Promise<void>>();

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

async function authorizedAccount(
  resource: ResourceSnapshot,
  context: PluginContext,
  rejectedToken?: string,
): Promise<AccountData> {
  const previous = refreshQueues.get(resource.id) ?? Promise.resolve();
  let release!: () => void;
  const queued = new Promise<void>((resolve) => {
    release = resolve;
  });
  refreshQueues.set(resource.id, queued);
  await previous;
  try {
    context.signal.throwIfAborted();
    if (!context.resource) throw new Error("Codex requires a selected account resource");
    const current = await context.resource.read();
    const data = accountData(current);
    const expiresAtMs = tokenExpiresAtMs(data.accessToken);
    const rejected = rejectedToken === data.accessToken;
    if (!rejected && (expiresAtMs === null || expiresAtMs > Date.now() + REFRESH_MARGIN_MS)) {
      return data;
    }
    if (!data.refreshToken) {
      if (rejected || (expiresAtMs !== null && expiresAtMs <= Date.now())) {
        throw new CodexAuthorizationError();
      }
      return data;
    }
    const response = await context.network.fetch(OAUTH_TOKEN_URL, {
      method: "POST",
      sensitive: true,
      headers: {
        accept: "application/json",
        "content-type": "application/x-www-form-urlencoded",
      },
      body: new URLSearchParams({
        grant_type: "refresh_token",
        client_id: CLIENT_ID,
        refresh_token: data.refreshToken,
      }).toString(),
    });
    let body: Record<string, unknown> = {};
    try {
      const parsed = JSON.parse(response.body);
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) body = parsed;
    } catch {
      // 不传播凭证端点的原文,它可能包含 token。
    }
    if (response.status < 200 || response.status >= 300) {
      const error = body.error;
      const code = typeof error === "string"
        ? error
        : text((error as Record<string, unknown> | null)?.code);
      if (
        response.status === 401 ||
        (response.status === 400 && [
          "invalid_grant",
          "refresh_token_expired",
          "refresh_token_reused",
          "refresh_token_invalidated",
        ].includes(code ?? ""))
      ) {
        throw new CodexAuthorizationError();
      }
      throw new Error(`Codex token refresh failed (HTTP ${response.status}); try again later`);
    }
    const accessToken = text(body.access_token);
    if (!accessToken) throw new Error("Codex token refresh returned no access token");
    const refreshed: AccountData = {
      ...data,
      accessToken,
      refreshToken: text(body.refresh_token) ?? data.refreshToken,
      accountId: chatGptAccountId(accessToken) ?? data.accountId,
    };
    await context.resource.patch({
      privateDataFields: {
        accessToken: refreshed.accessToken,
        refreshToken: refreshed.refreshToken,
        accountId: refreshed.accountId,
      },
      state: quotaState(refreshed.quota),
    });
    return refreshed;
  } finally {
    release();
    if (refreshQueues.get(resource.id) === queued) refreshQueues.delete(resource.id);
  }
}

/** 仅重试 HTTP 401,且最多一次;流已经输出时由调用方禁止重试。 */
export async function withAccountAuth<T>(
  resource: ResourceSnapshot,
  context: PluginContext,
  operation: (data: AccountData) => Promise<T>,
  canRetry: () => boolean = () => true,
): Promise<T> {
  try {
    let data = await authorizedAccount(resource, context);
    try {
      return await operation(data);
    } catch (error) {
      if (!(error instanceof HttpError) || error.status !== 401 || !canRetry()) throw error;
      data = await authorizedAccount(resource, context, data.accessToken);
      try {
        return await operation(data);
      } catch (retryError) {
        if (retryError instanceof HttpError && retryError.status === 401) {
          throw new CodexAuthorizationError();
        }
        throw retryError;
      }
    }
  } catch (error) {
    if (error instanceof CodexAuthorizationError && context.resource) {
      await context.resource.patch({ state: { status: "invalid", message: error.message } });
    }
    throw error;
  }
}

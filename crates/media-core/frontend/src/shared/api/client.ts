import { computed, ref } from "vue";

import { ApiError, type AuthTokensResponse, type CurrentSession } from "@/shared/api/types";

export const THEME_STORAGE_KEY = "streamserver.console.theme";
export const REFRESH_TOKEN_STORAGE_KEY = "streamserver.console.refresh_token";

const accessToken = ref<string>("");
const currentSession = ref<CurrentSession | null>(null);
let refreshAccessPromise: Promise<void> | null = null;

export const sessionToken = computed(() => accessToken.value);
export const activeSession = computed(() => currentSession.value);

export function setAccessToken(token: string) {
  accessToken.value = token;
}

export function clearAccessToken() {
  accessToken.value = "";
}

export function setCurrentSession(session: CurrentSession | null) {
  currentSession.value = session;
}

export interface RequestOptions {
  method?: string;
  body?: unknown;
  headers?: HeadersInit;
  skipAuth?: boolean;
}

export interface UploadRequestOptions {
  headers?: HeadersInit;
  onProgress?: (progress: { loaded: number; total: number | null; percent: number | null }) => void;
  skipAuth?: boolean;
}

async function parseResponsePayload<T>(response: Response): Promise<T | null> {
  const contentType = response.headers.get("content-type") ?? "";
  return response.status === 204
    ? null
    : contentType.includes("application/json")
      ? ((await response.json()) as T)
      : ((await response.text()) as T);
}

function messageFromPayload(payload: unknown, fallback: string) {
  return typeof payload === "object" && payload && "message" in payload
    ? String((payload as { message?: unknown }).message ?? fallback)
    : fallback;
}

async function sendApiRequest<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const headers = new Headers(options.headers ?? {});
  if (!options.skipAuth && accessToken.value) {
    headers.set("Authorization", `Bearer ${accessToken.value}`);
  }

  let body = options.body;
  if (
    body &&
    typeof body === "object" &&
    !(body instanceof Blob) &&
    !(body instanceof FormData)
  ) {
    headers.set("Content-Type", "application/json");
    body = JSON.stringify(body);
  }

  const response = await fetch(path, {
    method: options.method ?? "GET",
    headers,
    body: body as BodyInit | null | undefined,
  });

  const payload = await parseResponsePayload<T>(response);

  if (!response.ok) {
    const message = messageFromPayload(payload, `HTTP ${response.status}`);
    throw new ApiError(message, response.status, (payload as Record<string, unknown>) ?? undefined);
  }

  return payload as T;
}

async function refreshAccessToken() {
  const refreshToken = readRefreshToken();
  if (!refreshToken) {
    throw new ApiError("登录已过期，请重新登录", 403);
  }

  if (!refreshAccessPromise) {
    refreshAccessPromise = (async () => {
      const response = await fetch("/api/v1/auth/refresh", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ refresh_token: refreshToken }),
      });
      const payload = await parseResponsePayload<AuthTokensResponse>(response);

      if (!response.ok || !payload?.access_token) {
        clearSessionTokens();
        const message = messageFromPayload(payload, `HTTP ${response.status}`);
        throw new ApiError(message, response.status, (payload as unknown as Record<string, unknown>) ?? undefined);
      }

      setAccessToken(payload.access_token);
      if (payload.refresh_token) {
        writeRefreshToken(payload.refresh_token);
      }
    })().finally(() => {
      refreshAccessPromise = null;
    });
  }

  return refreshAccessPromise;
}

function shouldRefreshAfterError(path: string, options: RequestOptions, error: unknown) {
  return (
    error instanceof ApiError &&
    error.status === 403 &&
    !options.skipAuth &&
    Boolean(readRefreshToken()) &&
    path !== "/api/v1/auth/refresh"
  );
}

function clearSessionTokens() {
  clearAccessToken();
  writeRefreshToken("");
  setCurrentSession(null);
}

export async function apiRequest<T>(path: string, options: RequestOptions = {}): Promise<T> {
  try {
    return await sendApiRequest<T>(path, options);
  } catch (error) {
    if (!shouldRefreshAfterError(path, options, error)) {
      throw error;
    }

    await refreshAccessToken();
    return sendApiRequest<T>(path, options);
  }
}

export function uploadFormData<T>(
  path: string,
  form: FormData,
  options: UploadRequestOptions = {},
): Promise<T> {
  const headers = new Headers(options.headers ?? {});
  if (!options.skipAuth && accessToken.value) {
    headers.set("Authorization", `Bearer ${accessToken.value}`);
  }

  return new Promise((resolve, reject) => {
    const request = new XMLHttpRequest();
    request.open("POST", path);
    headers.forEach((value, key) => {
      request.setRequestHeader(key, value);
    });
    request.responseType = "text";

    request.upload.onprogress = (event) => {
      if (event.lengthComputable && event.total > 0) {
        options.onProgress?.({
          loaded: event.loaded,
          total: event.total,
          percent: Math.min(100, Math.round((event.loaded / event.total) * 100)),
        });
        return;
      }
      options.onProgress?.({
        loaded: event.loaded,
        total: null,
        percent: null,
      });
    };

    request.onerror = () => reject(new ApiError("网络请求失败", 0));
    request.ontimeout = () => reject(new ApiError("上传请求超时", 0));
    request.onabort = () => reject(new ApiError("上传已取消", 0));
    request.onload = () => {
      const contentType = request.getResponseHeader("content-type") ?? "";
      const text = request.responseText ?? "";
      let payload: unknown = null;
      try {
        payload = contentType.includes("application/json") && text ? JSON.parse(text) : text;
      } catch {
        payload = text;
      }

      if (request.status < 200 || request.status >= 300) {
        const message =
          typeof payload === "object" && payload && "message" in payload
            ? String((payload as { message?: unknown }).message ?? `HTTP ${request.status}`)
            : `HTTP ${request.status}`;
        reject(new ApiError(message, request.status, (payload as Record<string, unknown>) ?? undefined));
        return;
      }

      resolve(payload as T);
    };

    request.send(form);
  });
}

export function readRefreshToken() {
  return globalThis.localStorage?.getItem(REFRESH_TOKEN_STORAGE_KEY) ?? "";
}

export function writeRefreshToken(token: string) {
  if (token) {
    globalThis.localStorage?.setItem(REFRESH_TOKEN_STORAGE_KEY, token);
  } else {
    globalThis.localStorage?.removeItem(REFRESH_TOKEN_STORAGE_KEY);
  }
}

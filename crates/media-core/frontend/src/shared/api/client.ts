import { computed, ref } from "vue";

import { ApiError, type CurrentSession } from "@/shared/api/types";

export const THEME_STORAGE_KEY = "streamserver.console.theme";
export const REFRESH_TOKEN_STORAGE_KEY = "streamserver.console.refresh_token";

const accessToken = ref<string>("");
const currentSession = ref<CurrentSession | null>(null);

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

export async function apiRequest<T>(path: string, options: RequestOptions = {}): Promise<T> {
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

  const contentType = response.headers.get("content-type") ?? "";
  const payload =
    response.status === 204
      ? null
      : contentType.includes("application/json")
        ? ((await response.json()) as T)
        : ((await response.text()) as T);

  if (!response.ok) {
    const message =
      (payload as { message?: string } | null)?.message ?? `HTTP ${response.status}`;
    throw new ApiError(message, response.status, (payload as Record<string, unknown>) ?? undefined);
  }

  return payload as T;
}

export function readRefreshToken() {
  return window.localStorage.getItem(REFRESH_TOKEN_STORAGE_KEY) ?? "";
}

export function writeRefreshToken(token: string) {
  if (token) {
    window.localStorage.setItem(REFRESH_TOKEN_STORAGE_KEY, token);
  } else {
    window.localStorage.removeItem(REFRESH_TOKEN_STORAGE_KEY);
  }
}

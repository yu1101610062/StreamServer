import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  REFRESH_TOKEN_STORAGE_KEY,
  apiRequest,
  clearAccessToken,
  setAccessToken,
} from "@/shared/api/client";

function jsonResponse(payload: unknown, status = 200) {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function createStorage() {
  const values = new Map<string, string>();
  return {
    getItem: vi.fn((key: string) => values.get(key) ?? null),
    setItem: vi.fn((key: string, value: string) => {
      values.set(key, value);
    }),
    removeItem: vi.fn((key: string) => {
      values.delete(key);
    }),
  };
}

describe("apiRequest auth refresh", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    clearAccessToken();
  });

  it("refreshes an expired access token and retries the original request", async () => {
    const storage = createStorage();
    storage.setItem(REFRESH_TOKEN_STORAGE_KEY, "refresh-token-1");
    vi.stubGlobal("localStorage", storage);

    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ message: "invalid bearer token" }, 403))
      .mockResolvedValueOnce(jsonResponse({
        access_token: "access-token-2",
        refresh_token: "refresh-token-2",
      }))
      .mockResolvedValueOnce(jsonResponse({ items: [], page: 1, page_size: 20, total: 0 }));
    vi.stubGlobal("fetch", fetchMock);

    setAccessToken("access-token-1");

    const result = await apiRequest("/api/v1/tasks");

    expect(result).toEqual({ items: [], page: 1, page_size: 20, total: 0 });
    expect(fetchMock).toHaveBeenCalledTimes(3);
    expect(fetchMock.mock.calls[0]?.[1]?.headers.get("Authorization")).toBe("Bearer access-token-1");
    expect(fetchMock.mock.calls[1]?.[0]).toBe("/api/v1/auth/refresh");
    expect(fetchMock.mock.calls[1]?.[1]?.body).toBe(JSON.stringify({ refresh_token: "refresh-token-1" }));
    expect(fetchMock.mock.calls[2]?.[1]?.headers.get("Authorization")).toBe("Bearer access-token-2");
    expect(storage.setItem).toHaveBeenCalledWith(REFRESH_TOKEN_STORAGE_KEY, "refresh-token-2");
  });
});

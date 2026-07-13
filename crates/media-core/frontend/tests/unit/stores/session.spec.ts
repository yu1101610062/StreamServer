import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError, type CurrentSession } from "@/shared/api/types";

const clientMocks = vi.hoisted(() => ({
  clearAccessToken: vi.fn(),
  readRefreshToken: vi.fn(),
  setAccessToken: vi.fn(),
  setCurrentSession: vi.fn(),
  writeRefreshToken: vi.fn(),
}));

const authMocks = vi.hoisted(() => ({
  currentSession: vi.fn(),
  refresh: vi.fn(),
  login: vi.fn(),
  logout: vi.fn(),
  changePassword: vi.fn(),
}));

vi.mock("@/shared/api/client", () => ({
  clearAccessToken: clientMocks.clearAccessToken,
  readRefreshToken: clientMocks.readRefreshToken,
  setAccessToken: clientMocks.setAccessToken,
  setCurrentSession: clientMocks.setCurrentSession,
  writeRefreshToken: clientMocks.writeRefreshToken,
}));

vi.mock("@/shared/api/resources", () => ({
  authApi: {
    currentSession: authMocks.currentSession,
    refresh: authMocks.refresh,
    login: authMocks.login,
    logout: authMocks.logout,
    changePassword: authMocks.changePassword,
  },
}));

import { useSessionStore } from "@/stores/session";

describe("session store initialize", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.clearAllMocks();
    clientMocks.readRefreshToken.mockReturnValue("refresh-token-1");
  });

  it("dedupes concurrent refresh attempts during startup", async () => {
    const currentSession: CurrentSession = {
      auth_enabled: true,
      auth_mode: "local",
      subject: "admin",
      role: "admin",
      must_change_password: false,
      permissions: ["task_read"],
      environment: "test",
    };
    authMocks.currentSession
      .mockRejectedValueOnce(new ApiError("forbidden", 403))
      .mockResolvedValueOnce(currentSession);
    authMocks.refresh.mockResolvedValueOnce({
      access_token: "access-token-2",
      refresh_token: "refresh-token-2",
    });

    const store = useSessionStore();

    await Promise.all([store.initialize(), store.initialize()]);

    expect(authMocks.refresh).toHaveBeenCalledTimes(1);
    expect(authMocks.refresh).toHaveBeenCalledWith("refresh-token-1");
    expect(authMocks.currentSession).toHaveBeenCalledTimes(2);
    expect(clientMocks.setAccessToken).toHaveBeenCalledWith("access-token-2");
    expect(clientMocks.writeRefreshToken).toHaveBeenCalledWith("refresh-token-2");
    expect(clientMocks.clearAccessToken).not.toHaveBeenCalled();
    expect(store.session).toEqual(currentSession);
    expect(store.isAuthenticated).toBe(true);
    expect(store.loading).toBe(false);
    expect(store.error).toBeNull();
  });

  it("logs out with the latest persisted refresh token", async () => {
    clientMocks.readRefreshToken.mockReturnValueOnce("refresh-token-in-store");
    authMocks.login.mockResolvedValueOnce({
      access_token: "access-token-1",
      refresh_token: "refresh-token-in-store",
    });
    authMocks.currentSession.mockResolvedValueOnce({
      auth_enabled: true,
      auth_mode: "local",
      subject: "admin",
      role: "admin",
      must_change_password: false,
      permissions: ["task_read"],
      environment: "test",
    } satisfies CurrentSession);
    authMocks.logout.mockResolvedValueOnce(null);

    const store = useSessionStore();
    await store.login("admin", "secret");

    clientMocks.readRefreshToken.mockReturnValueOnce("refresh-token-rotated-by-client");
    await store.logout();

    expect(authMocks.logout).toHaveBeenCalledWith("refresh-token-rotated-by-client", {
      skipAuth: true,
    });
    expect(clientMocks.clearAccessToken).toHaveBeenCalled();
    expect(clientMocks.writeRefreshToken).toHaveBeenCalledWith("");
  });

  it("clears stale local tokens even when remote logout fails", async () => {
    clientMocks.readRefreshToken.mockReturnValueOnce("already-revoked-refresh-token");
    authMocks.logout.mockRejectedValueOnce(new ApiError("already revoked", 403));
    const store = useSessionStore();

    await expect(store.logout()).rejects.toMatchObject({ status: 403 });

    expect(clientMocks.clearAccessToken).toHaveBeenCalled();
    expect(clientMocks.writeRefreshToken).toHaveBeenCalledWith("");
    expect(store.session).toBeNull();
  });
});

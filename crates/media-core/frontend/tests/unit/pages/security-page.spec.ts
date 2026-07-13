// @vitest-environment jsdom

import { VueQueryPlugin } from "@tanstack/vue-query";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { CurrentSession } from "@/shared/api/types";

const apiMocks = vi.hoisted(() => ({
  currentSession: vi.fn(),
  refresh: vi.fn(),
  login: vi.fn(),
  logout: vi.fn(),
  changePassword: vi.fn(),
  listMachineAllowlist: vi.fn(),
  updateMachineAllowlist: vi.fn(),
}));
const clientMocks = vi.hoisted(() => ({
  clearAccessToken: vi.fn(),
  setCurrentSession: vi.fn(),
  writeRefreshToken: vi.fn(),
}));
const routerMocks = vi.hoisted(() => ({ push: vi.fn() }));

vi.mock("vue-router", () => ({
  useRouter: () => routerMocks,
}));

vi.mock("@/shared/api/client", () => ({
  clearAccessToken: clientMocks.clearAccessToken,
  readRefreshToken: vi.fn(() => "refresh-token"),
  setAccessToken: vi.fn(),
  setCurrentSession: clientMocks.setCurrentSession,
  writeRefreshToken: clientMocks.writeRefreshToken,
}));

vi.mock("@/shared/api/resources", () => ({
  authApi: {
    currentSession: apiMocks.currentSession,
    refresh: apiMocks.refresh,
    login: apiMocks.login,
    logout: apiMocks.logout,
    changePassword: apiMocks.changePassword,
  },
  securityApi: {
    listMachineAllowlist: apiMocks.listMachineAllowlist,
    updateMachineAllowlist: apiMocks.updateMachineAllowlist,
  },
}));

import SecurityPage from "@/pages/SecurityPage.vue";
import { useSessionStore } from "@/stores/session";

const elementStubs = {
  "el-button": { template: "<button><slot /></button>" },
  "el-form": { template: "<form><slot /></form>" },
  "el-form-item": { template: "<label><slot /></label>" },
  "el-input": { template: "<input />" },
};

describe("SecurityPage must-change mode", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    setActivePinia(createPinia());
  });

  it("shows only password change and never requests the machine allowlist", async () => {
    const store = useSessionStore();
    store.$patch({
      loading: false,
      session: {
        auth_enabled: true,
        auth_mode: "local_password",
        subject: "bootstrap-admin",
        role: "admin",
        must_change_password: true,
        permissions: [],
        environment: "test",
      } satisfies CurrentSession,
    });

    const wrapper = mount(SecurityPage, {
      global: {
        plugins: [VueQueryPlugin],
        stubs: elementStubs,
      },
    });
    await flushPromises();

    expect(wrapper.text()).toContain("修改当前密码");
    expect(wrapper.text()).not.toContain("机器 API 白名单");
    expect(apiMocks.listMachineAllowlist).not.toHaveBeenCalled();
  });

  it("clears the stale local session and returns to login after changing the password", async () => {
    const store = useSessionStore();
    store.$patch({
      loading: false,
      session: {
        auth_enabled: true,
        auth_mode: "local_password",
        subject: "bootstrap-admin",
        role: "admin",
        must_change_password: true,
        permissions: [],
        environment: "test",
      } satisfies CurrentSession,
    });
    apiMocks.changePassword.mockResolvedValueOnce(null);
    apiMocks.logout.mockRejectedValueOnce(new Error("refresh session already revoked"));
    const wrapper = mount(SecurityPage, {
      global: { plugins: [VueQueryPlugin], stubs: elementStubs },
    });

    await wrapper.find("button").trigger("click");
    await flushPromises();

    expect(apiMocks.changePassword).toHaveBeenCalledTimes(1);
    expect(apiMocks.logout).toHaveBeenCalledWith("refresh-token", { skipAuth: true });
    expect(clientMocks.clearAccessToken).toHaveBeenCalled();
    expect(clientMocks.writeRefreshToken).toHaveBeenCalledWith("");
    expect(store.session).toBeNull();
    expect(routerMocks.push).toHaveBeenCalledWith("/login");
  });
});

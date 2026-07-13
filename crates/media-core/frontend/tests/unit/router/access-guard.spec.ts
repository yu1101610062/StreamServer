import { describe, expect, it } from "vitest";

import { routeAccessRedirect } from "@/router/access-guard";

describe("routeAccessRedirect", () => {
  it("forces a must-change session into the password-change page", () => {
    expect(
      routeAccessRedirect({
        destinationName: "overview",
        destinationPath: "/overview",
        isPublic: false,
        isAuthenticated: true,
        mustChangePassword: true,
        permissions: [],
      }),
    ).toBe("/security");
    expect(
      routeAccessRedirect({
        destinationName: "security",
        destinationPath: "/security",
        isPublic: false,
        isAuthenticated: true,
        mustChangePassword: true,
        requiredPermission: "security_write",
        permissions: [],
      }),
    ).toBeUndefined();
    expect(
      routeAccessRedirect({
        destinationName: "api-docs",
        destinationPath: "/api-docs",
        isPublic: true,
        isAuthenticated: true,
        mustChangePassword: true,
        permissions: [],
      }),
    ).toBe("/security");
  });

  it("does not let an ordinary external principal bypass security_write", () => {
    expect(
      routeAccessRedirect({
        destinationName: "security",
        destinationPath: "/security",
        isPublic: false,
        isAuthenticated: true,
        mustChangePassword: false,
        requiredPermission: "security_write",
        permissions: ["task_read"],
      }),
    ).toBe("/overview");
  });
});

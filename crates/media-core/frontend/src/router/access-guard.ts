export interface RouteAccessContext {
  destinationName?: string | symbol | null;
  destinationPath: string;
  isPublic: boolean;
  isAuthenticated: boolean;
  mustChangePassword: boolean;
  requiredPermission?: string | null;
  permissions: readonly string[];
}

export type RouteAccessRedirect =
  | string
  | { path: string; query: { next: string } }
  | undefined;

export function routeAccessRedirect(context: RouteAccessContext): RouteAccessRedirect {
  if (context.isAuthenticated && context.mustChangePassword) {
    return context.destinationName === "security" ? undefined : "/security";
  }
  if (context.isPublic) {
    return undefined;
  }
  if (!context.isAuthenticated) {
    return {
      path: "/login",
      query: { next: context.destinationPath },
    };
  }
  if (
    context.requiredPermission &&
    !context.permissions.includes(context.requiredPermission)
  ) {
    return "/overview";
  }
  return undefined;
}

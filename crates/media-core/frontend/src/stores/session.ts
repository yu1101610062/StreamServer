import { computed, ref } from "vue";
import { defineStore } from "pinia";

import {
  clearAccessToken,
  readRefreshToken,
  setAccessToken,
  setCurrentSession,
  writeRefreshToken,
} from "@/shared/api/client";
import { authApi } from "@/shared/api/resources";
import type { ApiError, CurrentSession } from "@/shared/api/types";

export const useSessionStore = defineStore("session", () => {
  const session = ref<CurrentSession | null>(null);
  const loading = ref(true);
  const error = ref<ApiError | null>(null);
  const refreshToken = ref(readRefreshToken());
  let initializePromise: Promise<void> | null = null;

  async function fetchSession() {
    const current = await authApi.currentSession();
    session.value = current;
    setCurrentSession(current);
    error.value = null;
    return current;
  }

  async function initialize() {
    if (initializePromise) {
      return initializePromise;
    }
    initializePromise = (async () => {
      loading.value = true;
      try {
        await fetchSession();
      } catch (cause) {
        const authError = cause as ApiError;
        if (authError.status === 403 && refreshToken.value) {
          try {
            const tokens = await authApi.refresh(refreshToken.value);
            setAccessToken(tokens.access_token);
            if (tokens.refresh_token) {
              refreshToken.value = tokens.refresh_token;
              writeRefreshToken(tokens.refresh_token);
            }
            await fetchSession();
          } catch (refreshError) {
            clearSession();
            error.value = refreshError as ApiError;
          }
        } else {
          clearSession();
          error.value = authError;
        }
      } finally {
        loading.value = false;
        initializePromise = null;
      }
    })();
    return initializePromise;
  }

  async function login(username: string, password: string) {
    const tokens = await authApi.login({ username, password });
    setAccessToken(tokens.access_token);
    refreshToken.value = tokens.refresh_token ?? "";
    writeRefreshToken(tokens.refresh_token ?? "");
    await fetchSession();
  }

  async function useBearerToken(token: string) {
    setAccessToken(token);
    refreshToken.value = "";
    writeRefreshToken("");
    await fetchSession();
  }

  async function logout() {
    const currentRefreshToken = readRefreshToken();
    if (currentRefreshToken) {
      await authApi.logout(currentRefreshToken, { skipAuth: true });
    }
    clearSession();
  }

  function clearSession() {
    clearAccessToken();
    refreshToken.value = "";
    writeRefreshToken("");
    session.value = null;
    setCurrentSession(null);
  }

  function hasPermission(permission?: string | null) {
    if (!permission) {
      return true;
    }
    if (!session.value) {
      return false;
    }
    return session.value.permissions.includes(permission);
  }

  return {
    session,
    loading,
    error,
    initialize,
    login,
    useBearerToken,
    logout,
    hasPermission,
    isAuthenticated: computed(() => Boolean(session.value)),
  };
});

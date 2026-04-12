import { computed, ref } from "vue";
import { defineStore } from "pinia";

import { THEME_STORAGE_KEY } from "@/shared/api/client";

export type ThemePreference = "system" | "light" | "dark";

function readThemePreference(): ThemePreference {
  const value = (window.localStorage.getItem(THEME_STORAGE_KEY) ?? "system") as ThemePreference;
  return value === "light" || value === "dark" ? value : "system";
}

function resolveTheme(preference: ThemePreference) {
  if (preference === "light" || preference === "dark") {
    return preference;
  }
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function applyTheme(preference: ThemePreference) {
  const theme = resolveTheme(preference);
  document.documentElement.dataset.theme = theme;
  document.documentElement.style.colorScheme = theme;
}

export const useThemeStore = defineStore("theme", () => {
  const preference = ref<ThemePreference>(readThemePreference());

  function setPreference(next: ThemePreference) {
    preference.value = next;
    window.localStorage.setItem(THEME_STORAGE_KEY, next);
    applyTheme(next);
  }

  function initialize() {
    applyTheme(preference.value);
    const mediaQuery = window.matchMedia("(prefers-color-scheme: dark)");
    const handleChange = () => {
      if (preference.value === "system") {
        applyTheme("system");
      }
    };
    if (typeof mediaQuery.addEventListener === "function") {
      mediaQuery.addEventListener("change", handleChange);
    } else {
      mediaQuery.addListener(handleChange);
    }
  }

  return {
    preference,
    activeTheme: computed(() => resolveTheme(preference.value)),
    initialize,
    setPreference,
  };
});

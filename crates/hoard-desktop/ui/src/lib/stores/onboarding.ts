/**
 * Persisted wizard state.
 *
 * If the user closes the app halfway through onboarding we bring them back
 * to the same step on the next launch. Storage lives in
 * `tauri-plugin-store` (a JSON file managed by Tauri), not in the browser's
 * localStorage — we want it to survive a webview cache wipe.
 */

import { LazyStore } from "@tauri-apps/plugin-store";

const STORE_FILE = "onboarding.json";
const KEY_STEP = "step";
const KEY_URL = "url";
/** Whether the post-onboarding app tour has already been shown. Persisted so
 *  the guided tour only pops once — the first time the user finishes signing
 *  in — and never nags on later launches. */
const KEY_TOUR_DONE = "tour_done";

/** Routes that make up the wizard, in order.
 *
 * The path forks at `choose`: the Cloud branch is `language → choose → terms`
 * (3 steps), the self-hosted branch is `language → choose → server → token →
 * done` (5 steps). `WizardShell` sizes the progress dots per branch. */
export type OnboardingStep =
  | "language"
  | "choose"
  | "terms"
  | "server"
  | "token"
  | "done";

const STEPS: OnboardingStep[] = [
  "language",
  "choose",
  "terms",
  "server",
  "token",
  "done",
];

const store = new LazyStore(STORE_FILE);

export async function loadStep(): Promise<OnboardingStep> {
  const raw = await store.get<string>(KEY_STEP);
  if (raw && STEPS.includes(raw as OnboardingStep)) {
    return raw as OnboardingStep;
  }
  return "language";
}

export async function saveStep(step: OnboardingStep): Promise<void> {
  await store.set(KEY_STEP, step);
  await store.save();
}

export async function loadUrl(): Promise<string> {
  return (await store.get<string>(KEY_URL)) ?? "";
}

export async function saveUrl(url: string): Promise<void> {
  await store.set(KEY_URL, url);
  await store.save();
}

/** Wipe wizard state — call this after a successful login. Does NOT clear the
 *  tour flag: logging out and back in shouldn't replay the tour. */
export async function clearOnboarding(): Promise<void> {
  await store.delete(KEY_STEP);
  await store.delete(KEY_URL);
  await store.save();
}

/** Has the guided app tour been shown yet? */
export async function loadTourDone(): Promise<boolean> {
  return (await store.get<boolean>(KEY_TOUR_DONE)) ?? false;
}

/** Mark the guided app tour as seen so it never auto-opens again. */
export async function markTourDone(): Promise<void> {
  await store.set(KEY_TOUR_DONE, true);
  await store.save();
}

export function routeForStep(step: OnboardingStep): string {
  switch (step) {
    case "language":
      return "/onboarding/language";
    case "choose":
      return "/onboarding/choose";
    case "terms":
      return "/onboarding/terms";
    case "server":
      return "/onboarding/server";
    case "token":
      return "/onboarding/token";
    case "done":
      return "/onboarding/done";
  }
}

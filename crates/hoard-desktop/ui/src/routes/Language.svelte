<script lang="ts">
  /**
   * Onboarding step 1 — language picker.
   *
   * Replaces the old static "Welcome" screen. Picking a language persists it
   * (via `setLocale`, which writes the agent prefs) and advances to the
   * sign-in chooser. The choice is not final — Settings → Language changes it
   * later — so we let the user continue with whatever is already active too.
   */
  import { push } from "svelte-spa-router";
  import { ArrowRight } from "lucide-svelte";
  import { fly } from "svelte/transition";
  import { _, locale } from "svelte-i18n";
  import Button from "../lib/components/Button.svelte";
  import WizardShell from "../lib/components/WizardShell.svelte";
  import { supportedLocales, setLocale } from "../lib/i18n";
  import { saveStep } from "../lib/stores/onboarding";

  let busy = $state(false);

  async function pick(code: string) {
    if (busy) return;
    busy = true;
    try {
      if (code !== $locale) await setLocale(code);
    } finally {
      busy = false;
    }
  }

  async function next() {
    await saveStep("choose");
    push("/onboarding/choose");
  }
</script>

<WizardShell step="language">
  <div in:fly={{ x: 24, duration: 220 }}>
    <h1 class="text-xl font-semibold tracking-tight text-zinc-50">
      {$_("onboarding.language_title")}
    </h1>
    <p class="mt-2 text-sm text-zinc-400">
      {$_("onboarding.language_subtitle")}
    </p>

    <div class="mt-6 grid grid-cols-2 gap-2">
      {#each supportedLocales as l (l.code)}
        <button
          type="button"
          onclick={() => pick(l.code)}
          disabled={busy}
          aria-pressed={$locale === l.code}
          class="rounded-xl border px-4 py-3 text-left text-sm font-medium transition focus:outline-none focus-visible:ring-2 focus-visible:ring-emerald-400/50 disabled:opacity-60 {$locale ===
          l.code
            ? 'border-emerald-500 bg-emerald-600/15 text-emerald-50'
            : 'border-zinc-700 bg-zinc-900 text-zinc-200 hover:border-zinc-500 hover:bg-zinc-800'}"
        >
          {l.label}
        </button>
      {/each}
    </div>

    <div class="mt-8 flex justify-end">
      <Button variant="primary" size="lg" onclick={next}>
        {$_("common.continue")}
        <ArrowRight size={16} />
      </Button>
    </div>
  </div>
</WizardShell>

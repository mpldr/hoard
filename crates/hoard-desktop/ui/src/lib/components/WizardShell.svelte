<script lang="ts">
  /**
   * Common chrome for every onboarding screen: vertically-centred card,
   * progress dots, optional back button, and a slot-rendered step name.
   *
   * Keeps each individual route (Welcome / ServerSetup / TokenSetup /
   * OnboardingDone) focused on its own form rather than re-implementing
   * navigation wiring.
   */
  import type { Snippet } from "svelte";
  import { ArrowLeft } from "lucide-svelte";
  import { _ } from "svelte-i18n";
  import Logo from "./Logo.svelte";
  import type { OnboardingStep } from "../stores/onboarding";

  type Props = {
    step: OnboardingStep;
    onBack?: () => void;
    children: Snippet;
  };

  let { step, onBack, children }: Props = $props();

  // The wizard forks at `choose`. Until the user picks a branch we show the
  // shorter (Cloud) 3-dot track; the self-hosted branch expands to 5 dots
  // only once the user is inside its server/token/done steps. This kills the
  // old bug where 5 dots always showed and jumped from step 2 straight home.
  const CLOUD_ORDER: OnboardingStep[] = ["language", "choose", "terms"];
  const SELFHOST_ORDER: OnboardingStep[] = [
    "language",
    "choose",
    "server",
    "token",
    "done",
  ];
  const order = $derived(
    step === "server" || step === "token" || step === "done"
      ? SELFHOST_ORDER
      : CLOUD_ORDER,
  );
  const currentIndex = $derived(order.indexOf(step));
</script>

<div
  class="flex min-h-full flex-col items-center justify-center bg-zinc-950 px-6 py-10"
>
  <div class="w-full max-w-md">
    <!-- Logo header: logo sits just left of the welcome wordmark so the two
         read as one unit (replaces the old standalone welcome screen). -->
    <div class="mb-8 flex items-center justify-center gap-3">
      <span class="text-emerald-500">
        <Logo size={40} />
      </span>
      <span class="text-xl font-semibold tracking-tight text-zinc-50">
        {$_("welcome.title")}
      </span>
    </div>

    <!-- Card — frosted glass matching the app shell. The `card-glass` class
         drops the blur on Linux (webkit2gtk black-rect bug). -->
    <div
      class="card-glass rounded-2xl border border-white/[0.08] bg-zinc-950/30 p-8 shadow-[0_1px_0_0_rgba(255,255,255,0.08)_inset,0_10px_40px_-12px_rgba(0,0,0,0.75)] backdrop-blur-xl"
    >
      {#if onBack}
        <button
          type="button"
          onclick={onBack}
          class="mb-4 inline-flex items-center gap-1 text-sm text-zinc-400 hover:text-zinc-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-[var(--color-accent)]/50 rounded"
        >
          <ArrowLeft size={14} />
          {$_("common.back")}
        </button>
      {/if}
      {@render children()}
    </div>

    <!-- Progress dots -->
    <div
      class="mt-6 flex items-center justify-center gap-2"
      aria-label={$_("wizard.step_aria", { values: { current: currentIndex + 1, total: order.length } })}
    >
      {#each order as s, i (s)}
        <span
          class="h-1.5 rounded-full transition-all duration-300 {i ===
          currentIndex
            ? 'w-6 bg-emerald-500'
            : i < currentIndex
              ? 'w-1.5 bg-emerald-500/70'
              : 'w-1.5 bg-zinc-700'}"
          aria-hidden="true"
        ></span>
      {/each}
    </div>
  </div>
</div>

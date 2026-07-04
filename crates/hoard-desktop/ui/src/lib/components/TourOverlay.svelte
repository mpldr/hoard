<script lang="ts">
  /**
   * Guided app tour — shown once, right after the user finishes signing in for
   * the first time. Walks through each section of the app (including the Pro
   * features Hoard Screen and Hoard Wrapped) with a fluid step transition.
   *
   * Controls: "Omitir" (red, leaves early) and "Continuar" (green, advances);
   * the final step's primary button reads "Empezar" and closes the tour. The
   * parent owns the persisted "seen" flag via `onClose`, so this component is
   * purely presentational.
   */
  import { fly } from "svelte/transition";
  import {
    Home,
    Boxes,
    History,
    RotateCw,
    MonitorPlay,
    Sparkles,
    Settings as SettingsIcon,
  } from "lucide-svelte";
  import { _ } from "svelte-i18n";
  import Button from "./Button.svelte";

  type Props = { onClose: () => void };
  let { onClose }: Props = $props();

  type Step = {
    icon: typeof Home;
    titleKey: string;
    bodyKey: string;
    pro?: boolean;
  };

  const steps: Step[] = [
    { icon: Home, titleKey: "tour.dashboard_title", bodyKey: "tour.dashboard_body" },
    { icon: Boxes, titleKey: "tour.library_title", bodyKey: "tour.library_body" },
    { icon: History, titleKey: "tour.history_title", bodyKey: "tour.history_body" },
    { icon: RotateCw, titleKey: "tour.automatic_title", bodyKey: "tour.automatic_body" },
    { icon: MonitorPlay, titleKey: "tour.screen_title", bodyKey: "tour.screen_body", pro: true },
    { icon: Sparkles, titleKey: "tour.wrapped_title", bodyKey: "tour.wrapped_body", pro: true },
    { icon: SettingsIcon, titleKey: "tour.settings_title", bodyKey: "tour.settings_body" },
  ];

  let i = $state(0);
  const step = $derived(steps[i]);
  const isLast = $derived(i === steps.length - 1);
  const StepIcon = $derived(step.icon);

  function next() {
    if (isLast) onClose();
    else i += 1;
  }
</script>

<div
  class="fixed inset-0 z-[100] flex items-center justify-center bg-zinc-950/80 p-6 backdrop-blur-sm"
  role="dialog"
  aria-modal="true"
  aria-label={$_("tour.aria")}
>
  <div
    class="w-full max-w-md rounded-2xl border border-zinc-800 bg-zinc-900/95 p-8 shadow-2xl"
  >
    {#key i}
      <div in:fly={{ x: 28, duration: 240 }}>
        <div class="flex items-center gap-3">
          <span
            class="inline-flex h-12 w-12 shrink-0 items-center justify-center rounded-xl bg-emerald-600/15 text-emerald-400"
          >
            <StepIcon size={24} />
          </span>
          <div class="min-w-0">
            <h2 class="text-lg font-semibold tracking-tight text-zinc-50">
              {$_(step.titleKey)}
            </h2>
            {#if step.pro}
              <span
                class="mt-0.5 inline-block rounded-full bg-emerald-500/15 px-2 py-0.5 text-[11px] font-semibold uppercase tracking-wide text-emerald-400"
              >
                {$_("tour.pro_badge")}
              </span>
            {/if}
          </div>
        </div>
        <p class="mt-4 text-sm leading-relaxed text-zinc-300">
          {$_(step.bodyKey)}
        </p>
      </div>
    {/key}

    <!-- Progress dots -->
    <div class="mt-6 flex items-center justify-center gap-2">
      {#each steps as _s, idx (idx)}
        <span
          class="h-1.5 rounded-full transition-all duration-300 {idx === i
            ? 'w-6 bg-emerald-500'
            : idx < i
              ? 'w-1.5 bg-emerald-500/70'
              : 'w-1.5 bg-zinc-700'}"
          aria-hidden="true"
        ></span>
      {/each}
    </div>

    <div class="mt-8 flex items-center justify-between">
      <button
        type="button"
        onclick={onClose}
        class="rounded-lg px-3 py-2 text-sm font-medium text-red-400 transition hover:bg-red-500/10 hover:text-red-300 focus:outline-none focus-visible:ring-2 focus-visible:ring-red-500/50"
      >
        {$_("tour.skip")}
      </button>
      <Button variant="primary" size="lg" onclick={next}>
        {isLast ? $_("tour.start") : $_("tour.next")}
      </Button>
    </div>
  </div>
</div>

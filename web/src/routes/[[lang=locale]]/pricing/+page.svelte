<script lang="ts">
  import { _ } from 'svelte-i18n';
  import Seo from '$lib/components/Seo.svelte';
  import Button from '$lib/components/Button.svelte';
  import { PLANS, formatPlanQuota, formatMaxSaveSize } from '$lib/plans';
  import type { BillingCycle, PlanId } from '$lib/types';
  import { goto } from '$app/navigation';
  import { reveal } from '$lib/actions/reveal';
  import { get } from 'svelte/store';
  import { localeHref } from '$lib/i18n/href';
  import { Check, Receipt, Unlock, HelpCircle, Sparkles } from 'lucide-svelte';

  let cycle = $state<BillingCycle>('monthly');

  let monthlyBtn = $state<HTMLButtonElement | null>(null);
  let yearlyBtn = $state<HTMLButtonElement | null>(null);
  let trackEl = $state<HTMLDivElement | null>(null);

  function choose(plan: PlanId) {
    if (plan === 'free') {
      goto(get(localeHref)('/download'));
      return;
    }
    goto(`/checkout?plan=${plan}&cycle=${cycle}`);
  }

  const proPrice = $derived(cycle === 'monthly' ? PLANS.pro.priceMonthly : PLANS.pro.priceYearly);
  const proPriceLabel = $derived(
    `${proPrice.toLocaleString('es-ES', { minimumFractionDigits: 2 })} €`
  );
  const proSuffix = $derived(cycle === 'monthly' ? $_('pricing.per_month') : $_('pricing.per_year'));
  // Struck full-year price when yearly is selected (two months free).
  const fullYearLabel = $derived(
    `${(PLANS.pro.priceMonthly * 12).toLocaleString('es-ES', { minimumFractionDigits: 2 })} €`
  );

  // Slider thumb geometry, measured off the real button rects.
  let thumbStyle = $state('opacity:0;');
  function measure() {
    if (!trackEl || !monthlyBtn || !yearlyBtn) return;
    const trackRect = trackEl.getBoundingClientRect();
    const target = cycle === 'monthly' ? monthlyBtn : yearlyBtn;
    const r = target.getBoundingClientRect();
    thumbStyle = `left:${r.left - trackRect.left}px; width:${r.width}px;`;
  }
  $effect(() => {
    void cycle;
    void trackEl;
    void monthlyBtn;
    void yearlyBtn;
    measure();
  });
  $effect(() => {
    if (typeof window === 'undefined') return;
    const onResize = () => measure();
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  });

  type Cell = { kind: 'text'; value: string } | { kind: 'check' } | { kind: 'none' };
  type Row = { label: string; brand?: boolean; info?: string; free: Cell; pro: Cell };

  const rows: Row[] = $derived([
    {
      label: $_('pricing.row_storage'),
      free: { kind: 'text', value: formatPlanQuota('free') },
      pro: { kind: 'text', value: formatPlanQuota('pro') }
    },
    {
      label: $_('pricing.row_devices'),
      free: { kind: 'text', value: '3' },
      pro: { kind: 'text', value: $_('pricing.val_unlimited') }
    },
    {
      label: $_('pricing.row_history'),
      free: { kind: 'check' },
      pro: { kind: 'check' }
    },
    {
      label: $_('pricing.row_save_size'),
      free: { kind: 'text', value: formatMaxSaveSize('free') },
      pro: { kind: 'text', value: formatMaxSaveSize('pro') }
    },
    {
      label: $_('pricing.row_sync'),
      free: { kind: 'check' },
      pro: { kind: 'check' }
    },
    {
      label: $_('pricing.row_export'),
      free: { kind: 'check' },
      pro: { kind: 'check' }
    },
    {
      label: 'Hoard Wrapped',
      brand: true,
      info: 'pricing.wrapped_body',
      free: { kind: 'none' },
      pro: { kind: 'check' }
    },
    {
      label: 'Hoard Screen',
      brand: true,
      info: 'pricing.screen_body',
      free: { kind: 'none' },
      pro: { kind: 'check' }
    }
  ]);

  const notes = [
    { icon: Check, title: 'pricing.note_cancel_title', body: 'pricing.note_cancel_body' },
    { icon: Receipt, title: 'pricing.note_mor_title', body: 'pricing.note_mor_body' },
    { icon: Unlock, title: 'pricing.note_lockin_title', body: 'pricing.note_lockin_body' }
  ];
</script>

<Seo path="/pricing" key="pricing" />

<section class="mx-auto max-w-5xl px-4 py-16 sm:px-6 sm:py-24">
  <div class="mx-auto max-w-2xl text-center">
    <p class="kicker justify-center">{$_('nav.pricing')}</p>
    <h1 class="mt-3 text-balance text-4xl font-semibold text-ink sm:text-5xl">
      {$_('pricing.title')}
    </h1>
    <p class="mt-4 text-pretty leading-relaxed text-ink-soft">
      {$_('pricing.subtitle')}
    </p>
  </div>

  <!-- Billing toggle -->
  <div class="mt-10 flex flex-col items-center gap-3">
    <div
      bind:this={trackEl}
      class="relative inline-flex items-center rounded-full border border-line bg-surface p-1"
      role="tablist"
      aria-label="Billing cycle"
    >
      <span
        class="absolute inset-y-1 rounded-full bg-accent-deep transition-[left,width] duration-300 ease-out"
        aria-hidden="true"
        style={thumbStyle}
      ></span>
      <button
        bind:this={monthlyBtn}
        role="tab"
        aria-selected={cycle === 'monthly'}
        class="relative z-10 rounded-full px-6 py-1.5 text-sm font-medium ring-focus transition-colors duration-300 {cycle ===
        'monthly'
          ? 'text-white'
          : 'text-ink-soft hover:text-ink'}"
        onclick={() => (cycle = 'monthly')}
      >
        {$_('pricing.toggle_monthly')}
      </button>
      <button
        bind:this={yearlyBtn}
        role="tab"
        aria-selected={cycle === 'yearly'}
        class="relative z-10 rounded-full px-6 py-1.5 text-sm font-medium ring-focus transition-colors duration-300 {cycle ===
        'yearly'
          ? 'text-white'
          : 'text-ink-soft hover:text-ink'}"
        onclick={() => (cycle = 'yearly')}
      >
        {$_('pricing.toggle_yearly')}
      </button>
    </div>
    <span
      class="inline-flex items-center gap-1.5 rounded-full border border-accent/30 bg-accent-tint px-2.5 py-0.5 text-[11px] font-semibold text-accent transition-opacity duration-300"
      style="opacity: {cycle === 'yearly' ? '1' : '0.55'};"
    >
      {$_('pricing.yearly_badge')}
    </span>
  </div>

  {#snippet val(cell: Cell, pro: boolean)}
    {#if cell.kind === 'text'}
      <span class="font-mono text-xs {pro ? 'font-medium text-accent' : 'text-ink-soft'}"
        >{cell.value}</span
      >
    {:else if cell.kind === 'check'}
      <Check class="h-4 w-4 shrink-0 {pro ? 'text-accent' : 'text-ink-soft'}" />
    {:else}
      <span class="h-1.5 w-1.5 shrink-0 rounded-full bg-ink-faint/50"></span>
    {/if}
  {/snippet}

  <!-- MOBILE: stacked plan cards -->
  <div class="reveal mt-12 grid gap-5 sm:hidden" use:reveal>
    <!-- Free card -->
    <div class="relative overflow-hidden rounded-2xl border border-accent/25 bg-accent-tint/40 p-5">
      <div
        class="pointer-events-none absolute inset-0 bg-gradient-to-b from-accent/[0.06] to-transparent"
      ></div>
      <div class="relative flex items-baseline justify-between gap-3">
        <h3 class="font-display text-lg font-semibold text-ink">Hoard Free</h3>
        <span class="text-sm text-ink-soft">{$_('pricing.free_forever')}</span>
      </div>
      <ul class="relative mt-4 space-y-3 text-sm">
        {#each rows as row (row.label)}
          <li class="flex items-center justify-between gap-3">
            <span class={row.brand && row.free.kind === 'none' ? 'text-ink-faint' : 'text-ink-soft'}>
              {row.label}
            </span>
            {#if row.brand && row.free.kind === 'none'}
              <span
                class="shrink-0 rounded-full bg-bg px-2 py-0.5 font-mono text-[10px] uppercase tracking-wide text-ink-faint"
              >
                {$_('pricing.pro_only')}
              </span>
            {:else}
              {@render val(row.free, false)}
            {/if}
          </li>
        {/each}
      </ul>
      <div class="relative mt-5">
        <Button variant="secondary" full onclick={() => choose('free')}>
          {$_('pricing.cta_download_free')}
        </Button>
      </div>
    </div>

    <!-- Pro card (highlighted) — gradient border on all 4 sides, hermetic -->
    <div
      class="relative rounded-2xl bg-gradient-to-br from-accent via-accent-deep to-accent p-[1.5px] shadow-xl shadow-accent-deep/40"
    >
      <div class="relative overflow-hidden rounded-[15px] bg-accent-tint p-5">
        <div
          class="pointer-events-none absolute inset-0 bg-gradient-to-b from-accent/[0.12] via-accent/[0.03] to-transparent"
        ></div>
        <div
          class="pointer-events-none absolute -right-20 -top-20 h-44 w-44 rounded-full bg-accent/20 blur-3xl"
        ></div>
      <div class="relative flex items-start justify-between gap-3">
        <h3 class="flex items-center gap-1.5 font-display text-lg font-semibold text-ink">
          <Sparkles class="h-4 w-4 text-accent" />Hoard Pro
        </h3>
        <div class="text-right">
          <div class="flex items-baseline justify-end gap-1">
            <span class="font-display text-2xl font-bold tabular-nums text-ink">{proPriceLabel}</span>
            <span class="text-xs text-ink-soft">{proSuffix}</span>
          </div>
          {#if cycle === 'yearly'}
            <s class="text-xs tabular-nums text-ink-faint">{fullYearLabel}</s>
          {/if}
        </div>
      </div>
      <ul class="relative mt-4 space-y-3 text-sm">
        {#each rows as row (row.label)}
          <li>
            <div class="flex items-center justify-between gap-3">
              <span class={row.brand ? 'font-medium text-ink' : 'text-ink-soft'}>{row.label}</span>
              {@render val(row.pro, true)}
            </div>
            {#if row.brand && row.info}
              <p class="mt-1 text-xs leading-relaxed text-ink-faint">{$_(row.info)}</p>
            {/if}
          </li>
        {/each}
      </ul>
      <div class="relative mt-5">
        <Button variant="primary" full onclick={() => choose('pro')}>
          {$_('pricing.cta_buy_pro')}
        </Button>
      </div>
      </div>
    </div>
  </div>

  <!-- DESKTOP: single comparison grid; Pro column rendered as a filled,
       glowing rectangle that spans the whole column. -->
  <div class="reveal mx-auto mt-12 hidden max-w-3xl sm:block" use:reveal>
    <div class="relative grid grid-cols-[1.5fr_1fr_1.15fr]">
      <!-- Background highlight: fills the Pro column with a gradient-bordered
           green panel across every row. -->
      <div
        class="pointer-events-none absolute inset-0 grid grid-cols-[1.5fr_1fr_1.15fr]"
        aria-hidden="true"
      >
        <div class="col-span-2 rounded-l-2xl border border-r-0 border-accent/15 bg-accent-tint/40"></div>
        <div
          class="rounded-2xl bg-gradient-to-br from-accent via-accent-deep to-accent p-[1.5px] shadow-xl shadow-accent-deep/40"
        >
          <div class="h-full w-full rounded-[15px] bg-accent-tint"></div>
        </div>
      </div>

      <!-- Header row -->
      <div class="relative z-10 border-b border-line"></div>
      <div class="relative z-10 flex flex-col gap-2 border-b border-l border-line p-5 text-center">
        <h3 class="font-display text-lg font-semibold text-ink">Hoard Free</h3>
        <p class="text-xs text-ink-soft">{$_('pricing.free_forever')}</p>
        <div class="mt-auto pt-2">
          <Button variant="secondary" full onclick={() => choose('free')}>
            {$_('pricing.cta_download_free')}
          </Button>
        </div>
      </div>
      <div class="relative z-10 flex flex-col gap-2 p-5 text-center">
        <h3
          class="flex items-center justify-center gap-1.5 font-display text-lg font-semibold text-ink"
        >
          <Sparkles class="h-4 w-4 text-accent" />Hoard Pro
        </h3>
        <div class="flex flex-wrap items-baseline justify-center gap-x-1.5 gap-y-0.5">
          <span class="font-display text-3xl font-bold tabular-nums text-ink">{proPriceLabel}</span>
          <span class="text-xs text-ink-soft">{proSuffix}</span>
          {#if cycle === 'yearly'}
            <s class="w-full text-xs tabular-nums text-ink-faint">{fullYearLabel}</s>
          {/if}
        </div>
        <div class="mt-auto pt-2">
          <Button variant="primary" full onclick={() => choose('pro')}>
            {$_('pricing.cta_buy_pro')}
          </Button>
        </div>
      </div>

      <!-- Feature rows (each emits 3 grid cells) -->
      {#each rows as row, i (row.label)}
        {@const last = i === rows.length - 1}
        <div
          class="relative z-10 px-5 py-3 text-sm {last ? '' : 'border-b border-line'} {row.brand
            ? 'font-medium text-ink'
            : 'text-ink-soft'}"
        >
          {#if row.brand}
            <span class="inline-flex items-center gap-1.5">
              <span class="h-1.5 w-1.5 rounded-full bg-accent"></span>{row.label}
              {#if row.info}
                <button
                  type="button"
                  class="ring-focus inline-grid h-4 w-4 place-items-center rounded-full text-ink-faint transition-colors hover:text-accent"
                  title={$_(row.info)}
                  aria-label={$_(row.info)}
                >
                  <HelpCircle class="h-3.5 w-3.5" />
                </button>
              {/if}
            </span>
          {:else}
            {row.label}
          {/if}
        </div>
        <div
          class="relative z-10 flex items-center justify-center border-l border-line px-2 py-3 text-sm {last
            ? ''
            : 'border-b'}"
        >
          {@render val(row.free, false)}
        </div>
        <div class="relative z-10 flex items-center justify-center px-2 py-3 text-sm">
          {@render val(row.pro, true)}
        </div>
      {/each}
    </div>
  </div>

  <!-- Notes -->
  <div class="mt-16 grid gap-5 text-sm sm:grid-cols-3">
    {#each notes as n, i (n.title)}
      <div class="reveal rounded-2xl border border-line bg-surface p-6" use:reveal={{ delay: i * 80 }}>
        <div class="flex items-center gap-2.5">
          <span class="grid h-8 w-8 place-items-center rounded-lg bg-accent-tint text-accent">
            <n.icon class="h-4 w-4" />
          </span>
          <h3 class="font-semibold text-ink">{$_(n.title)}</h3>
        </div>
        <p class="mt-3 leading-relaxed text-ink-soft">{$_(n.body)}</p>
      </div>
    {/each}
  </div>
</section>

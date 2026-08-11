<script lang="ts">
  import { _ } from 'svelte-i18n';
  import Button from '$lib/components/Button.svelte';
  import Seo from '$lib/components/Seo.svelte';
  import HowItWorks from '$lib/components/HowItWorks.svelte';
  import DownloadCTA from '$lib/components/DownloadCTA.svelte';
  import { reveal } from '$lib/actions/reveal';
  import { tilt } from '$lib/actions/tilt';
  import { localeHref } from '$lib/i18n/href';
  import { SITE_URL } from '$lib/i18n/locales';
  import { PLANS } from '$lib/plans';
  import { version } from '$lib/version';
  import {
    ArrowRight,
    RefreshCw,
    History,
    Fingerprint,
    ScanSearch,
    MonitorSmartphone,
    FileDown,
    Check,
    Heart
  } from 'lucide-svelte';


  const features = [
    { key: 'sync', icon: RefreshCw },
    { key: 'versioned', icon: History },
    { key: 'verified', icon: Fingerprint },
    { key: 'detect', icon: ScanSearch },
    { key: 'crossplat', icon: MonitorSmartphone },
    { key: 'export', icon: FileDown }
  ];

  // Twelve facts, all of them checkable in the repo — a marquee with four
  // items announces its own loop every few seconds.
  const facts = [
    'sha',
    'agpl',
    'eu',
    'os',
    'rust',
    'selfhost',
    'proton',
    'saves',
    'history',
    'export',
    'noads',
    'solo'
  ];

  // Structured data for rich results: the product + its two pricing tiers and
  // the operating organization. Description tracks the page locale.
  const jsonLd = $derived(
    `<script type="application/ld+json">${JSON.stringify({
      '@context': 'https://schema.org',
      '@graph': [
        {
          '@type': 'Organization',
          '@id': `${SITE_URL}/#org`,
          name: 'Hoard',
          url: SITE_URL,
          logo: `${SITE_URL}/icon.png`,
          founder: { '@type': 'Person', name: 'Raimundo León Oliva' }
        },
        {
          '@type': 'SoftwareApplication',
          name: 'Hoard',
          applicationCategory: 'UtilitiesApplication',
          operatingSystem: 'Windows, macOS, Linux',
          url: SITE_URL,
          description: $_('seo.home.desc'),
          publisher: { '@id': `${SITE_URL}/#org` },
          offers: [
            {
              '@type': 'Offer',
              name: $_('plan.free'),
              price: '0',
              priceCurrency: 'EUR'
            },
            {
              '@type': 'Offer',
              name: $_('plan.pro'),
              price: PLANS.pro.priceMonthly.toFixed(2),
              priceCurrency: 'EUR'
            }
          ]
        }
      ]
    })}<\/script>`
  );
</script>

<Seo path="/" key="home" />
<svelte:head>
  <link rel="preload" as="image" href="/WEB.png" fetchpriority="high" />
  <!-- eslint-disable-next-line svelte/no-at-html-tags -->
  {@html jsonLd}
</svelte:head>

<!-- ───────── HERO ───────── -->
<section class="relative">
  <div class="mx-auto max-w-6xl px-4 pt-16 sm:px-6 sm:pt-24">
    <div class="flex flex-col items-center text-center">
      <h1
        class="max-w-4xl text-balance text-[2.4rem] font-semibold leading-[1.05] text-ink sm:text-6xl lg:text-[4.4rem] animate-fade-up"
        style="animation-delay:0.05s"
      >
        {$_('hero.title_1')}
        <span class="text-accent">{$_('hero.title_2')}</span>
      </h1>

      <p
        class="mt-6 max-w-2xl text-pretty text-[1.05rem] leading-relaxed text-ink-soft sm:text-lg animate-fade-up"
        style="animation-delay:0.12s"
      >
        {$_('hero.subtitle')}
      </p>

      <div
        class="mt-9 flex flex-col items-center gap-4 sm:flex-row animate-fade-up"
        style="animation-delay:0.2s"
      >
        <Button href={$localeHref('/download')} size="lg" variant="primary">
          {$_('hero.cta_start')}
          <ArrowRight class="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
        </Button>
        <Button href={$localeHref('/pricing')} size="lg" variant="secondary">
          {$_('hero.cta_pricing')}
        </Button>
      </div>

      <p class="mt-4 font-mono text-xs tracking-wide text-ink-faint animate-fade-up" style="animation-delay:0.26s">
        {$_('hero.subnote', { values: { v: $version } })}
      </p>
    </div>
  </div>
</section>

<!-- ───────── SCREENSHOT ───────── -->
<section class="relative">
  <div class="relative mx-auto max-w-[84rem] px-4 pt-14 sm:px-6">
    <div class="flex flex-col items-center gap-8 lg:flex-row lg:items-center lg:justify-center lg:gap-7">
      <figure
        class="tilt animate-fade-up relative w-full lg:w-auto"
        style="animation-delay:0.3s"
        use:tilt
      >
        <img
          src="/WEB.png"
          alt={$_('hero.screenshot_alt')}
          width="1270"
          height="920"
          fetchpriority="high"
          decoding="async"
          class="block w-full rounded-2xl border border-line-strong shadow-[0_40px_90px_-40px_rgba(0,0,0,0.9)] lg:h-[22rem] lg:w-auto xl:h-[28rem] 2xl:h-[34rem]"
        />
      </figure>
      <figure
        class="tilt animate-fade-up relative w-full max-w-2xl lg:w-auto lg:max-w-none"
        style="animation-delay:0.42s"
        use:tilt
      >
        <img
          src="/CLI.png"
          alt={$_('hero.screenshot_cli_alt')}
          width="664"
          height="630"
          decoding="async"
          class="block w-full rounded-2xl border border-line-strong shadow-[0_40px_90px_-40px_rgba(0,0,0,0.9)] lg:h-[22rem] lg:w-auto xl:h-[28rem] 2xl:h-[34rem]"
        />
      </figure>
    </div>
  </div>
</section>

<!-- ───────── FACT STRIP ───────── -->
<section class="border-y border-line" aria-label={$_('facts.aria')}>
  <!-- The second track is the same list again: it fills the right edge while
       the first scrolls out, and it is aria-hidden so nothing reads twice. -->
  <div class="marquee">
    <div class="marquee-inner">
      {#each [false, true] as duplicate (duplicate)}
        <dl class="flex shrink-0 items-center" aria-hidden={duplicate || undefined}>
          {#each facts as f (f)}
            <div class="flex shrink-0 items-center gap-2.5 whitespace-nowrap px-7 py-5">
              <dt class="font-mono text-sm font-medium tracking-tight text-ink">
                {$_(`facts.${f}.value`)}
              </dt>
              <dd class="text-xs text-ink-faint">{$_(`facts.${f}.label`)}</dd>
              <span class="ml-4 h-1 w-1 shrink-0 rounded-full bg-accent/40" aria-hidden="true"
              ></span>
            </div>
          {/each}
        </dl>
      {/each}
    </div>
  </div>
</section>

<!-- ───────── SYNC ───────── -->
<section class="relative">
  <div class="mx-auto max-w-6xl px-4 py-20 sm:px-6 sm:py-24">
    <div class="grid items-center gap-12 lg:grid-cols-2">
      <div class="reveal" use:reveal>
        <p class="kicker">{$_('sync.kicker')}</p>
        <h2 class="mt-3 text-balance text-3xl font-semibold text-ink sm:text-4xl">
          {$_('sync.title')}
        </h2>
        <p class="mt-4 text-pretty leading-relaxed text-ink-soft">
          {$_('sync.body')}
        </p>
        <ul class="mt-6 space-y-3 text-sm">
          {#each ['p1', 'p2', 'p3'] as p (p)}
            <li class="flex items-start gap-2.5 text-ink-soft">
              <Check class="mt-0.5 h-4 w-4 flex-none text-accent" />
              {$_(`sync.${p}`)}
            </li>
          {/each}
        </ul>
        <a
          href={$localeHref('/guides')}
          class="link-underline mt-6 inline-flex items-center gap-1.5 text-sm font-medium text-accent ring-focus hover:text-emerald-300"
        >
          {$_('sync.guides_link')}
          <ArrowRight class="h-4 w-4" />
        </a>
      </div>

      <div class="reveal" use:reveal={{ delay: 100 }}>
        <div class="tilt relative rounded-2xl border border-line bg-surface p-6 sm:p-8" use:tilt={{ max: 5 }}>
          <div class="space-y-3 font-mono text-xs">
            <div class="flex items-center justify-between rounded-lg border border-line bg-bg px-4 py-3">
              <span class="text-ink">{$_('sync.demo_desktop')}</span>
              <span class="rounded-full bg-accent-tint px-2 py-0.5 text-[10px] text-accent">
                v48 · {$_('sync.demo_uploaded')}
              </span>
            </div>
            <div class="flex justify-center text-ink-faint" aria-hidden="true">
              <svg width="16" height="24" viewBox="0 0 16 24" fill="none">
                <path d="M8 0v20m0 0l-5-5m5 5l5-5" stroke="currentColor" stroke-width="1.5" />
              </svg>
            </div>
            <div class="flex items-center justify-between rounded-lg border border-accent bg-accent-tint px-4 py-3">
              <span class="text-ink">{$_('sync.demo_laptop')}</span>
              <span class="rounded-full bg-accent-deep px-2 py-0.5 text-[10px] text-white">
                v48 · {$_('sync.demo_synced')}
              </span>
            </div>
          </div>
          <p class="mt-5 text-center text-xs leading-relaxed text-ink-faint">
            {$_('sync.demo_note')}
          </p>
        </div>
      </div>
    </div>
  </div>
</section>

<!-- ───────── HOW IT WORKS ───────── -->
<HowItWorks />

<!-- ───────── FEATURES ───────── -->
<section class="border-t border-line">
  <div class="mx-auto max-w-6xl px-4 py-20 sm:px-6 sm:py-24">
    <div class="reveal max-w-2xl" use:reveal>
      <p class="kicker">{$_('features.kicker')}</p>
      <h2 class="mt-3 text-balance text-3xl font-semibold text-ink sm:text-4xl">
        {$_('features.title')}
      </h2>
      <p class="mt-3 text-pretty text-ink-soft">{$_('features.subtitle')}</p>
    </div>

    <div
      class="mt-12 grid overflow-hidden rounded-2xl border border-line bg-surface sm:grid-cols-2 lg:grid-cols-3"
    >
      {#each features as f, i (f.key)}
        <article
          class="reveal tilt group relative border-t border-line p-7 transition-colors hover:bg-bg max-sm:[&:nth-child(2n)]:border-l sm:[&:nth-child(-n+2)]:border-t-0 sm:[&:nth-child(2n)]:border-l lg:[&:nth-child(-n+3)]:border-t-0 lg:[&:nth-child(3n)]:border-l-0 lg:[&:nth-child(3n+1)]:border-l-0"
          use:reveal={{ delay: i * 60 }}
          use:tilt={{ max: 4 }}
        >
          <f.icon class="h-5 w-5 text-accent" />
          <h3 class="mt-4 font-semibold text-ink">{$_(`features.${f.key}.title`)}</h3>
          <p class="mt-1.5 text-sm leading-relaxed text-ink-soft">
            {$_(`features.${f.key}.body`)}
          </p>
        </article>
      {/each}
    </div>
  </div>
</section>

<!-- ───────── SELF-HOST ───────── -->
<section class="border-t border-line">
  <div class="mx-auto max-w-6xl px-4 py-20 sm:px-6">
    <div class="reveal overflow-hidden rounded-3xl border border-pine-line bg-pine" use:reveal>
      <div class="grid items-center gap-10 p-10 sm:p-14 lg:grid-cols-2">
        <div>
          <p class="kicker">{$_('selfhost.kicker')}</p>
          <h2 class="mt-3 text-balance text-3xl font-semibold text-white sm:text-4xl">
            {$_('selfhost.title')}
          </h2>
          <p class="mt-4 text-pretty leading-relaxed text-white/60">
            {$_('selfhost.body')}
          </p>
          <a
            href="https://github.com/rleeon/hoard#self-host"
            target="_blank"
            rel="noopener noreferrer"
            class="link-underline mt-6 inline-flex items-center gap-1.5 text-sm font-medium text-emerald-400 ring-focus hover:text-emerald-300"
          >
            {$_('selfhost.cta')}
            <ArrowRight class="h-4 w-4" />
          </a>
        </div>
        <pre
          class="overflow-x-auto rounded-xl border border-pine-line bg-black/30 p-5 font-mono text-[13px] leading-relaxed text-white/80"><code
            >git clone https://github.com/rleeon/hoard.git
cd hoard/deploy/docker
docker compose up -d --build</code
          ></pre>
      </div>
    </div>
  </div>
</section>

<!-- ───────── FINAL CTA ───────── -->
<section class="border-t border-line">
  <div class="mx-auto max-w-6xl px-4 py-20 sm:px-6 sm:py-24">
    <div class="reveal mx-auto flex max-w-2xl flex-col items-center text-center" use:reveal>
      <h2 class="text-balance text-3xl font-semibold text-ink sm:text-4xl">
        {$_('cta_section.title')}
      </h2>
      <p class="mt-3 text-pretty text-ink-soft">{$_('cta_section.body')}</p>
      <div class="mt-8">
        <DownloadCTA />
      </div>
    </div>
  </div>
</section>

<!-- ───────── SUPPORT ───────── -->
<section class="border-t border-line">
  <div class="mx-auto max-w-6xl px-4 py-16 sm:px-6">
    <div
      class="reveal mx-auto flex max-w-3xl flex-col items-center gap-5 rounded-xl border border-line bg-surface px-6 py-10 text-center"
      use:reveal
    >
      <div>
        <h2 class="font-display text-2xl font-semibold text-ink">{$_('support.title')}</h2>
        <p class="mt-2 text-pretty text-sm text-ink-soft">{$_('support.body')}</p>
      </div>
      <div class="flex flex-col items-stretch gap-3 sm:flex-row sm:items-center">
        <Button href="https://github.com/sponsors/rleeon" target="_blank" variant="outline">
          <Heart class="h-4 w-4" />
          {$_('support.cta')}
        </Button>
        <Button
          href="https://github.com/rleeon/hoard/blob/main/FUNDING.md"
          target="_blank"
          variant="secondary"
        >
          {$_('support.funding')}
        </Button>
      </div>
    </div>
  </div>
</section>

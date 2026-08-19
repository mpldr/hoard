<script lang="ts">
  import { _ } from 'svelte-i18n';
  import { reveal } from '$lib/actions/reveal';
  import DownloadCTA from '$lib/components/DownloadCTA.svelte';

  // The "Free and open source" closing CTA section, as the user picked it:
  // title, body and download buttons, centered. No kicker.
</script>

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

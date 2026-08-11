<script lang="ts">
  import { _ } from 'svelte-i18n';
  import Button from './Button.svelte';
  import { localeHref } from '$lib/i18n/href';
  import { onMount } from 'svelte';
  import { ArrowRight, Github } from 'lucide-svelte';

  type Platform = 'Windows' | 'macOS' | 'Linux';
  let label = $state<Platform | null>(null);

  onMount(() => {
    const ua = navigator.userAgent.toLowerCase();
    if (ua.includes('win')) label = 'Windows';
    else if (ua.includes('mac')) label = 'macOS';
    else if (ua.includes('linux')) label = 'Linux';
  });

  let cta = $derived(
    label
      ? $_('cta_section.cta_os', { values: { platform: label } })
      : $_('cta_section.cta')
  );
</script>

<div class="flex flex-col items-center gap-4">
  <div class="flex flex-col items-stretch gap-3 sm:flex-row sm:items-center">
    <Button href={$localeHref('/download')} size="lg" variant="primary">
      {cta}
      <ArrowRight class="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
    </Button>
    <Button
      href="https://github.com/rleeon/hoard/releases"
      target="_blank"
      size="lg"
      variant="secondary"
    >
      <Github class="h-4 w-4" />
      GitHub Releases
    </Button>
  </div>
  <p class="text-sm text-ink-faint">{$_('cta_section.subnote')}</p>
</div>

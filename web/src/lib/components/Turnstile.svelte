<script lang="ts">
  // Cloudflare Turnstile widget. Lazily loads the Cloudflare script (once per
  // page), renders an invisible-until-needed challenge and hands the resulting
  // token back to the parent. Rendering is fully gated by the caller: this
  // component is only mounted when a site key is configured, so when Turnstile
  // is disabled nothing here runs and the login flow is untouched.
  import { onMount, onDestroy } from 'svelte';

  interface Props {
    siteKey: string;
    // Called with the solved token, or '' when the token expires / errors and
    // the parent should treat the captcha as unsolved again.
    onToken: (token: string) => void;
    theme?: 'auto' | 'light' | 'dark';
  }

  let { siteKey, onToken, theme = 'auto' }: Props = $props();

  const SCRIPT_SRC =
    'https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit';

  type TurnstileApi = {
    render: (el: HTMLElement, opts: Record<string, unknown>) => string;
    reset: (id?: string) => void;
    remove: (id: string) => void;
  };
  // Cloudflare attaches the API to window.turnstile once the script loads.
  function api(): TurnstileApi | undefined {
    return (globalThis as unknown as { turnstile?: TurnstileApi }).turnstile;
  }

  let container: HTMLDivElement;
  let widgetId: string | null = null;

  function loadScript(): Promise<void> {
    if (api()) return Promise.resolve();
    return new Promise((resolve, reject) => {
      const existing = document.querySelector<HTMLScriptElement>(
        `script[src="${SCRIPT_SRC}"]`
      );
      if (existing) {
        existing.addEventListener('load', () => resolve(), { once: true });
        existing.addEventListener('error', () => reject(new Error('turnstile')), {
          once: true
        });
        return;
      }
      const s = document.createElement('script');
      s.src = SCRIPT_SRC;
      s.async = true;
      s.defer = true;
      s.onload = () => resolve();
      s.onerror = () => reject(new Error('turnstile'));
      document.head.appendChild(s);
    });
  }

  onMount(async () => {
    try {
      await loadScript();
    } catch {
      // Script blocked (adblock, offline). Leave the captcha unsolved; the
      // parent keeps the submit button disabled and the user can retry.
      return;
    }
    const t = api();
    if (!t || !container) return;
    widgetId = t.render(container, {
      sitekey: siteKey,
      theme,
      callback: (token: string) => onToken(token),
      'expired-callback': () => onToken(''),
      'error-callback': () => onToken('')
    });
  });

  onDestroy(() => {
    const t = api();
    if (t && widgetId) {
      try {
        t.remove(widgetId);
      } catch {
        // widget already gone
      }
    }
  });

  // Called by the parent after a failed/consumed submit to fetch a fresh token.
  export function reset() {
    const t = api();
    if (t && widgetId) t.reset(widgetId);
    onToken('');
  }
</script>

<div bind:this={container} class="flex justify-center"></div>

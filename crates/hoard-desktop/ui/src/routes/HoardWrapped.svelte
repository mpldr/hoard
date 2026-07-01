<script lang="ts">
  // Hoard-Wrapped entry. The Pro recap ships inside the official binary (`PRO`
  // true when the private `$pro` layer is linked), but only renders when the
  // server entitlement grants access — paid Pro or an active trial. The
  // `ProFeature` shell starts the one-week trial the first time this page is
  // opened and shows the days-left strip while it runs; Free-expired /
  // signed-out fall back to the gate inside it.
  import { Sparkles } from "lucide-svelte";
  import ProFeature from "../lib/components/ProFeature.svelte";
  import { PRO, Wrapped } from "$pro";
</script>

<ProFeature titleKey="hoard_wrapped.title" feature="wrapple" pro={PRO}>
  {#snippet icon()}
    <Sparkles size={30} class="text-emerald-400" />
  {/snippet}
  <Wrapped />
</ProFeature>

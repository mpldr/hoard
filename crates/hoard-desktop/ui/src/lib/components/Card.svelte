<script lang="ts">
  /** Visual container with the standard zinc surface + border treatment. */
  import type { Snippet } from "svelte";
  import { tilt } from "../actions/tilt";

  type Props = {
    children: Snippet;
    class?: string;
  };

  let { children, class: extraClass = "" }: Props = $props();
</script>

<!--
  Surface: a translucent obsidian panel lifted off the atmospheric
  background with a hairline top-highlight and a soft drop shadow, so
  cards read as layered glass rather than flat filled boxes. The `tilt`
  action adds a 3D lean + cursor-following glow on hover.
-->
<div
  class="tilt card-glass relative rounded-2xl border border-white/[0.09] bg-zinc-950/30 p-6
         shadow-[0_1px_0_0_rgba(255,255,255,0.08)_inset,0_10px_40px_-12px_rgba(0,0,0,0.75)]
         backdrop-blur-xl {extraClass}"
  use:tilt
>
  {@render children()}
</div>

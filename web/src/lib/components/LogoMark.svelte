<script lang="ts">
  interface Props {
    size?: number;
  }
  let { size = 28 }: Props = $props();
</script>

<svg
  xmlns="http://www.w3.org/2000/svg"
  viewBox="0 0 64 64"
  width={size}
  height={size}
  aria-hidden="true"
>
  <rect
    x="1"
    y="1"
    width="62"
    height="62"
    rx="13"
    fill="var(--color-pine-elev, #19231c)"
    stroke="var(--color-pine-line, #28332b)"
    stroke-width="2"
  />
  <rect x="9" y="9" width="14" height="46" rx="6" fill="#34d399" />
  <rect x="41" y="9" width="14" height="46" rx="6" fill="#34d399" />
  <rect x="9" y="25" width="46" height="14" rx="5" fill="#34d399" />
</svg>

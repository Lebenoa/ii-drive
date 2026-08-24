<script lang="ts">
  import { COUNTRIES, flagsRender, type Country } from '$lib/countries';
  import { pop } from '$lib/motion';

  /**
   * The selected country flows down; picking one flows up through
   * `onpick`. Not `$bindable`: the parent derives the country from what is
   * typed, so a two-way binding would fight that derivation.
   */
  let {
    country,
    disabled = false,
    onpick,
  }: { country: Country; disabled?: boolean; onpick: (c: Country) => void } = $props();

  let open = $state(false);
  let query = $state('');
  /** Index into `matches`, moved by the arrow keys and hover. */
  let active = $state(0);
  let rootEl = $state<HTMLElement | null>(null);
  let searchEl = $state<HTMLInputElement | null>(null);
  let listEl = $state<HTMLElement | null>(null);

  // Measured once: Windows has no flag glyphs, so a regional-indicator pair
  // renders as the two letters. Where that happens we show a deliberate ISO
  // chip instead of letters masquerading as a flag.
  const flags = flagsRender();

  const matches = $derived.by(() => {
    const q = query.trim().toLowerCase();
    if (q.length === 0) return COUNTRIES;
    // Leading "+" is how people write a dial code; the haystack holds both.
    const needle = q.startsWith('+') ? q.slice(1) : q;
    const starts = (c: Country) =>
      c.name.toLowerCase().startsWith(needle) ||
      c.iso2.toLowerCase() === needle ||
      c.dial.startsWith(needle);
    // Prefix hits first: typing "th" should offer Thailand before Lithuania.
    return [
      ...COUNTRIES.filter(starts),
      ...COUNTRIES.filter((c) => !starts(c) && c.search.includes(needle)),
    ];
  });

  function show(): void {
    if (disabled) return;
    open = true;
    query = '';
    active = Math.max(
      0,
      COUNTRIES.findIndex((c) => c.iso2 === country.iso2),
    );
    // Focus after the panel exists, so the caret lands in the search box.
    requestAnimationFrame(() => searchEl?.focus());
  }

  function choose(c: Country): void {
    onpick(c);
    hide();
  }

  function hide(): void {
    open = false;
    query = '';
  }

  /** Keeps the highlighted row inside the scroll port. */
  function scrollActiveIntoView(): void {
    requestAnimationFrame(() => {
      listEl?.querySelector('[data-active="true"]')?.scrollIntoView({ block: 'nearest' });
    });
  }

  function move(delta: number): void {
    if (matches.length === 0) return;
    // Wraps, so ArrowUp from the top jumps to the end.
    active = (active + delta + matches.length) % matches.length;
    scrollActiveIntoView();
  }

  function onKey(e: KeyboardEvent): void {
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault();
        if (open) move(1);
        else show();
        break;
      case 'ArrowUp':
        e.preventDefault();
        if (open) move(-1);
        break;
      case 'Home':
        if (open) {
          e.preventDefault();
          active = 0;
          scrollActiveIntoView();
        }
        break;
      case 'End':
        if (open) {
          e.preventDefault();
          active = matches.length - 1;
          scrollActiveIntoView();
        }
        break;
      case 'Enter':
        if (open) {
          // Inside a <form>: Enter picks the row, it must not submit.
          e.preventDefault();
          const hit = matches[active];
          if (hit) choose(hit);
        }
        break;
      case 'Escape':
        if (open) {
          e.preventDefault();
          e.stopPropagation();
          hide();
        }
        break;
      default:
        break;
    }
  }

  // Reset the highlight whenever the result set changes under it.
  $effect(() => {
    void query;
    active = 0;
  });
</script>

<!-- Light dismiss. `composedPath` covers clicks inside the panel. -->
<svelte:window
  onclick={(e) => {
    if (open && rootEl && !e.composedPath().includes(rootEl)) hide();
  }}
/>

<div class="wrap" bind:this={rootEl}>
  <button
    type="button"
    class="trigger"
    class:open
    {disabled}
    aria-haspopup="listbox"
    aria-expanded={open}
    aria-label={`Country code: ${country.name} +${country.dial}`}
    onclick={() => (open ? hide() : show())}
    onkeydown={(e) => {
      // Closed-state keys: open on the arrows. Open-state keys are handled
      // by the search field, which takes focus when the panel appears.
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        show();
      }
    }}
  >
    {#if flags}
      <span class="flag" aria-hidden="true">{country.flag}</span>
    {:else}
      <span class="iso" aria-hidden="true">{country.iso2}</span>
    {/if}
    <span class="dial">+{country.dial}</span>
    <span class="caret" aria-hidden="true">▾</span>
  </button>

  {#if open}
    <div class="panel" transition:pop={{ start: 0.97 }}>
      <input
        class="field search"
        type="text"
        placeholder="Search country or code…"
        autocomplete="off"
        spellcheck="false"
        bind:this={searchEl}
        bind:value={query}
        aria-label="Search countries"
        role="combobox"
        aria-expanded="true"
        aria-controls="country-list"
        onkeydown={onKey}
      />
      <div id="country-list" class="list" role="listbox" aria-label="Countries" bind:this={listEl}>
        {#each matches as c, i (c.iso2)}
          <button
            type="button"
            class="row"
            class:picked={c.iso2 === country.iso2}
            data-active={i === active}
            role="option"
            aria-selected={c.iso2 === country.iso2}
            onclick={() => choose(c)}
            onmousemove={() => (active = i)}
          >
            {#if flags}
              <span class="flag" aria-hidden="true">{c.flag}</span>
            {:else}
              <span class="iso" aria-hidden="true">{c.iso2}</span>
            {/if}
            <span class="name">{c.name}</span>
            <span class="muted code">+{c.dial}</span>
          </button>
        {:else}
          <p class="muted empty">No country matches “{query}”.</p>
        {/each}
      </div>
    </div>
  {/if}
</div>

<style>
  .wrap {
    position: relative;
    flex-shrink: 0;
  }

  /* Sized to sit flush with .field in the same row. */
  .trigger {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 100%;
    min-height: 38px;
    padding: 0 10px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: #10141c;
    color: var(--text);
    font: inherit;
    cursor: pointer;
    transition:
      border-color var(--dur-fast) var(--ease),
      box-shadow var(--dur-fast) var(--ease);
  }

  .trigger:hover:not(:disabled),
  .trigger.open {
    border-color: var(--accent);
  }

  .trigger:focus-visible {
    outline: none;
    box-shadow: 0 0 0 3px rgba(91, 157, 255, 0.35);
  }

  .trigger:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .flag {
    font-size: 17px;
    line-height: 1;
  }

  /* Flag stand-in where the platform lacks the glyphs. */
  .iso {
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.5px;
    color: var(--muted);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 1px 3px;
    line-height: 1.2;
  }

  .dial {
    font-variant-numeric: tabular-nums;
    font-size: 14px;
  }

  .caret {
    font-size: 10px;
    color: var(--muted);
  }

  .panel {
    position: absolute;
    z-index: 30;
    top: calc(100% + 6px);
    left: 0;
    width: min(320px, 78vw);
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 8px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    box-shadow: 0 18px 40px rgba(0, 0, 0, 0.45);
  }

  .search {
    font-size: 13.5px;
  }

  .list {
    max-height: 260px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .row {
    display: grid;
    grid-template-columns: 24px 1fr auto;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 7px 8px;
    border: 0;
    border-radius: 6px;
    background: none;
    color: var(--text);
    font: inherit;
    font-size: 13.5px;
    text-align: left;
    cursor: pointer;
  }

  /* Hover and keyboard share one highlight, so there is never a second
     "where am I" indicator competing with the first. */
  .row[data-active='true'] {
    background: var(--panel-2);
  }

  .row.picked {
    color: var(--accent);
  }

  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .code {
    font-variant-numeric: tabular-nums;
    font-size: 12.5px;
  }

  .empty {
    margin: 0;
    padding: 10px 8px;
    font-size: 13px;
  }
</style>

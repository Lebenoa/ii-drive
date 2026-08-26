<script lang="ts">
  import ShareVideoPlayer from '$lib/components/ShareVideoPlayer.svelte';
  import type { PageProps } from './$types';

  let { data }: PageProps = $props();
  const meta = $derived(data.meta);
  const raw = $derived(`/api/files/${encodeURIComponent(data.id)}/raw`);
  const thumb = $derived(`/api/files/${encodeURIComponent(data.id)}/thumb`);

  const isImage = $derived(meta.mime.startsWith('image/'));
  const isVideo = $derived(meta.mime.startsWith('video/'));
  const isAudio = $derived(meta.mime.startsWith('audio/'));

  function humanSize(bytes: number): string {
    const units = ['B', 'KB', 'MB', 'GB', 'TB'] as const;
    let v = bytes;
    let u = 0;
    while (v >= 1024 && u < units.length - 1) {
      v /= 1024;
      u += 1;
    }
    return u === 0 ? `${bytes} B` : `${v.toFixed(1)} ${units[u]}`;
  }
</script>

<svelte:head>
  <title>{meta.name}</title>
</svelte:head>

<main class="share">
  <div class="card">
    {#if isImage}
      <img class="media" src={raw} alt={meta.name} />
    {:else if isVideo}
      <ShareVideoPlayer src={raw} {thumb} mediaTitle={meta.name} />
    {:else if isAudio}
      <audio class="media" src={raw} controls></audio>
    {/if}
    <h1>{meta.name}</h1>
    {#if meta.owner}
      <p class="byline">shared by {meta.owner}</p>
    {/if}
    <p class="meta">{humanSize(meta.size)} · {meta.mime}</p>
    <a class="dl" href={`${raw}?dl=1`} download>Download</a>
  </div>
</main>

<style>
  /* Scoped to this component's own element — a `:global(body)` rule here
     would ride in on a stylesheet during client-side navigation and never
     unload, breaking the app layout at `/` until a hard refresh. */
  .share {
    margin: 0;
    min-height: 100vh;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    background: #101014;
    color: #e8e8ee;
    font-family: system-ui, sans-serif;
  }
  .card {
    max-width: 960px;
    width: calc(100% - 2rem);
    margin: 2rem 0;
    text-align: center;
  }
  .media {
    max-width: 100%;
    max-height: 78vh;
    border-radius: 10px;
  }
  h1 {
    font-size: 1.05rem;
    font-weight: 600;
    word-break: break-all;
    margin: 1rem 0 0.25rem;
  }
  .byline {
    margin: 0 0 0.25rem;
    color: #9a9aa6;
    font-size: 0.85rem;
  }
  p.meta {
    margin: 0 0 1.25rem;
    color: #9a9aa6;
    font-size: 0.85rem;
  }
  .dl {
    display: inline-block;
    padding: 0.55rem 1.4rem;
    border-radius: 8px;
    background: #3b82f6;
    color: #fff;
    text-decoration: none;
    font-size: 0.95rem;
  }
  .dl:hover {
    background: #2f6fe0;
  }
</style>

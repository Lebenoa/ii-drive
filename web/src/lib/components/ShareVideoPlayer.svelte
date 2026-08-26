<script lang="ts">
  /**
   * Custom video player for the public share page: thumbnail poster
   * (only when a thumbnail actually exists), big center play button and
   * a hover control bar — play/pause, seek, time, mute, fullscreen.
   */
  interface Props {
    src: string;
    thumb: string;
    mediaTitle: string;
  }

  let { src, thumb, mediaTitle }: Props = $props();
import { t } from '$lib/i18n.svelte';

  let video = $state<HTMLVideoElement | null>(null);
  let shell = $state<HTMLDivElement | null>(null);
  let playing = $state(false);
  let current = $state(0);
  let duration = $state(0);
  let muted = $state(false);
  let poster = $state('');
  let seeking = $state(false);
  let seekValue = $state(0);

  // A 404 thumbnail must not blank the first frame, so the poster is
  // attached only after the endpoint confirms it exists.
  $effect(() => {
    fetch(thumb, { method: 'HEAD' })
      .then((r) => {
        if (r.ok) poster = thumb;
      })
      .catch(() => {});
  });

  function fmt(s: number): string {
    if (!Number.isFinite(s)) return '0:00';
    s = Math.floor(s);
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  }

  function toggle(): void {
    if (!video) return;
    video.paused ? video.play() : video.pause();
  }

  function onSeekInput(e: Event): void {
    if (!video || !duration) return;
    const v = e.currentTarget as HTMLInputElement;
    video.currentTime = (Number(v.value) / 100) * duration;
  }

  function onTimeUpdate(): void {
    if (!video || seeking) return;
    current = video.currentTime;
    if (duration) seekValue = (current / duration) * 100;
  }

  function onFullscreen(): void {
    if (document.fullscreenElement) void document.exitFullscreen();
    else void shell?.requestFullscreen();
  }
</script>

<div class="player" bind:this={shell}>
  <!-- svelte-ignore a11y_media_has_caption -->
  <video
    bind:this={video}
    {src}
    {poster}
    preload="metadata"
    playsinline
    onclick={toggle}
    onplay={() => (playing = true)}
    onpause={() => (playing = false)}
    onloadedmetadata={() => (duration = video?.duration ?? 0)}
    ontimeupdate={onTimeUpdate}
  ></video>
  {#if !playing}
    <button class="bigplay" aria-label={t('player.play')} onclick={toggle}>▶</button>
  {/if}
  <div class="bar">
    <button type="button" aria-label={t('player.playPause')} onclick={toggle}>
      {playing ? '❚❚' : '▶'}
    </button>
    <span class="t">{fmt(current)}</span>
    <input
      type="range"
      min="0"
      max="100"
      step="0.05"
      bind:value={seekValue}
      oninput={onSeekInput}
      onpointerdown={() => (seeking = true)}
      onpointerup={() => (seeking = false)}
      aria-label={t('player.seek')}
    />
    <span class="t">{fmt(duration)}</span>
    <button
      type="button"
      aria-label={t('player.mute')}
      onclick={() => {
        if (!video) return;
        video.muted = !video.muted;
        muted = video.muted;
      }}
    >
      {muted ? '🔇' : '🔊'}
    </button>
    <button type="button" aria-label={t('player.fullscreen')} onclick={onFullscreen}>⛶</button>
  </div>
</div>

<style>
  .player {
    position: relative;
    display: inline-block;
    max-width: 100%;
    background: #000;
    border-radius: 10px;
    overflow: hidden;
  }
  video {
    display: block;
    max-width: 100%;
    max-height: 78vh;
  }
  .bigplay {
    position: absolute;
    inset: 0;
    margin: auto;
    width: 72px;
    height: 72px;
    border-radius: 50%;
    border: 0;
    cursor: pointer;
    background: rgba(59, 130, 246, 0.9);
    color: #fff;
    font-size: 1.7rem;
    line-height: 1;
  }
  .bigplay:hover {
    background: #3b82f6;
  }
  .bar {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.4rem 0.6rem;
    background: linear-gradient(transparent, rgba(0, 0, 0, 0.75));
    opacity: 0;
    transition: opacity 0.2s;
  }
  .player:hover .bar,
  .player:focus-within .bar {
    opacity: 1;
  }
  .bar button {
    background: none;
    border: 0;
    color: #fff;
    cursor: pointer;
    font-size: 1rem;
    padding: 0 0.2rem;
  }
  .t {
    font-size: 0.78rem;
    color: #ddd;
    min-width: 2.6rem;
  }
  .bar input[type='range'] {
    flex: 1;
    accent-color: #3b82f6;
  }
</style>

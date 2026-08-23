<svelte:head>
  <title>ii-drive — Internal DB</title>
</svelte:head>

<script lang="ts">
  import { goto } from '$app/navigation';
  import {
    dbQuery,
    dbTables,
    getToken,
    type DbQueryResult,
  } from '$lib/api';
  import { fadeUp, stagger } from '$lib/motion';

  let checking = $state(true);
  let tables = $state<string[]>([]);
  let active = $state('');
  let sql = $state('INFO FOR DB');
  let running = $state(false);
  let results = $state<DbQueryResult[]>([]);
  let error = $state('');

  // Row ids arrive as "table:id" strings — perfect for DELETE statements.
  let rows = $state<Record<string, unknown>[]>([]);

  $effect(() => {
    void (async () => {
      if (!getToken()) {
        goto('/login');
        return;
      }
      try {
        tables = (await dbTables()).tables;
      } catch (err) {
        error = err instanceof Error ? err.message : String(err);
      }
      checking = false;
    })();
  });

  async function run(statement: string): Promise<void> {
    if (running || !statement.trim()) return;
    running = true;
    error = '';
    try {
      const res = await dbQuery(statement);
      results = res.results;
      const failed = res.results.find((r) => !r.ok);
      if (failed) error = failed.error ?? 'query failed';
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      running = false;
    }
  }

  async function openTable(name: string): Promise<void> {
    active = name;
    await run(`SELECT * FROM ${name} LIMIT 200`);
  }

  async function deleteRow(id: string): Promise<void> {
    if (!confirm(`Delete record ${id}?`)) return;
    await run(`DELETE ${id}`);
    if (active) await openTable(active);
  }
</script>

<div class="db-shell">
  <header class="topbar">
    <a class="back" href="/">← Back to files</a>
    <span class="title">Internal DB <span class="muted">· drive.surrealkv</span></span>
    <span></span>
  </header>

  {#if checking}
    <div class="center-screen">
      <div class="spinner" aria-label="loading"></div>
    </div>
  {:else}
    <div class="cols">
      <nav class="side card" aria-label="Tables">
        <p class="side-head muted">Tables</p>
        <button
          class="tbl-btn"
          class:active={sql === 'INFO FOR DB'}
          type="button"
          onclick={() => void run('INFO FOR DB')}
        >
          schema
        </button>
        {#each tables as t, i (t)}
          <button
            class="tbl-btn"
            class:active={active === t}
            type="button"
            onclick={() => void openTable(t)}
            in:fadeUp={{ delay: stagger(i) }}
          >
            {t}
          </button>
        {/each}
      </nav>

      <main class="pane">
        <form
          class="editor"
          onsubmit={(e) => {
            e.preventDefault();
            void run(sql);
          }}
        >
          <textarea
            class="field sql"
            rows="3"
            spellcheck="false"
            placeholder="SurrealQL…"
            bind:value={sql}
            onkeydown={(e) => {
              if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
                e.preventDefault();
                void run(sql);
              }
            }}
          ></textarea>
          <button class="btn btn-primary" type="submit" disabled={running}>
            {#if running}<span class="spinner btn-spin"></span>{/if}
            {running ? 'Running…' : 'Run'}
          </button>
        </form>

        {#if error}<p class="error-text">{error}</p>{/if}

        {#each results as r, i (i)}
          {@const stmt = `#${i + 1}`}
          {#if r.ok}
            <section class="card result">
              <p class="muted res-head">{stmt}</p>
              {#if Array.isArray(r.result) && r.result.length > 0 && typeof r.result[0] === 'object' && r.result[0] !== null && 'id' in (r.result[0] as object)}
                <table class="rows">
                  <thead>
                    <tr><th>id</th><th>data</th><th></th></tr>
                  </thead>
                  <tbody>
                    {#each r.result as row, j (j)}
                      {@const rec = row as Record<string, unknown>}
                      <tr in:fadeUp={{ delay: stagger(j) }}>
                        <td class="mono">{String(rec.id)}</td>
                        <td class="mono data">{JSON.stringify(rec, null, 1)}</td>
                        <td>
                          <button
                            class="icon-btn danger"
                            type="button"
                            title="Delete record"
                            onclick={() => void deleteRow(String(rec.id))}
                          >
                            ✕
                          </button>
                        </td>
                      </tr>
                    {/each}
                  </tbody>
                </table>
              {:else}
                <pre class="mono raw">{JSON.stringify(r.result, null, 2)}</pre>
              {/if}
            </section>
          {:else}
            <section class="card result">
              <p class="error-text">{stmt}: {r.error}</p>
            </section>
          {/if}
        {/each}
      </main>
    </div>
  {/if}
</div>

<style>
  .db-shell {
    min-height: 100vh;
  }

  .cols {
    display: flex;
    gap: 14px;
    align-items: flex-start;
    padding: 14px 16px;
  }

  .side {
    width: 170px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 12px;
  }

  .side-head {
    margin: 0 0 4px;
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .tbl-btn {
    appearance: none;
    background: none;
    border: none;
    border-left: 2px solid transparent;
    color: var(--muted);
    font: inherit;
    font-size: 13px;
    text-align: left;
    padding: 5px 8px;
    cursor: pointer;
    transition:
      color var(--dur-fast) var(--ease),
      border-color var(--dur-fast) var(--ease);
  }

  .tbl-btn:hover {
    color: inherit;
  }

  .tbl-btn.active {
    color: inherit;
    border-left-color: var(--accent, currentColor);
    font-weight: 600;
  }

  .pane {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .editor {
    display: flex;
    gap: 8px;
    align-items: stretch;
  }

  textarea.sql {
    flex: 1;
    font-family: ui-monospace, monospace;
    font-size: 13px;
    resize: vertical;
  }

  .result {
    padding: 10px 12px;
    overflow-x: auto;
  }

  .res-head {
    margin: 0 0 6px;
    font-size: 12px;
  }

  .mono {
    font-family: ui-monospace, monospace;
    font-size: 12.5px;
  }

  table.rows {
    width: 100%;
    border-collapse: collapse;
  }

  table.rows th,
  table.rows td {
    text-align: left;
    padding: 4px 8px;
    border-bottom: 1px solid var(--border);
    vertical-align: top;
  }

  td.data {
    white-space: pre-wrap;
    word-break: break-all;
    color: var(--muted);
  }

  pre.raw {
    margin: 0;
    white-space: pre-wrap;
    word-break: break-all;
  }
</style>

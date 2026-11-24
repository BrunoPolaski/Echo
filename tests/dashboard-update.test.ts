import { expect, test } from 'bun:test';
import { clientHarness } from './helpers/mount-svelte';
import { sourcePath } from './helpers/render-svelte';
import { createUpdateController, updateAnnouncement, updateBusy, updateDescription, updateLabel, type UpdateView } from '../src/lib/ui/dashboard/nav/update-controller';
import type { UpdateClient, UpdateEnvelope } from '../src/lib/runtime/desktop/update-client';

const appVersion: string = (await Bun.file(new URL('../package.json', import.meta.url)).json()).version;

// The intact navigation inside a minimal dashboard context. `motion` mirrors DashboardApp's motionAllowed.
async function mountNav(options: { tauri: boolean }) {
  const harness = await clientHarness(`<script>
    import DashboardNav from '${sourcePath('src/lib/ui/dashboard/nav/DashboardNav.svelte')}';
    import { provideDashboardContext } from '${sourcePath('src/lib/ui/dashboard/context.ts')}';
    import { defaultSettings } from '${sourcePath('src/lib/core/settings.ts')}';
    provideDashboardContext({ settings: () => defaultSettings, permissions: () => undefined, permissionCheckState: () => 'checking' });
    let motion = $state(true);
    export const view = { motion: (next) => { motion = next; } };
  </script><DashboardNav motionAllowed={motion} activeTab="history" onselect={() => {}} />`);
  const commands: [string, unknown][] = [];
  const callbacks = new Map<number, (event: { payload: UpdateEnvelope }) => void>();
  let callbackId = 0, statusListener: number | undefined;
  let installResponse: UpdateEnvelope = { sequence: 0, status: { state: 'none' } };
  if (options.tauri) harness.install('isTauri', true);
  (harness.window as any).__TAURI_INTERNALS__ = {
    transformCallback(fn: (event: { payload: UpdateEnvelope }) => void) { callbacks.set(++callbackId, fn); return callbackId; },
    unregisterCallback(id: number) { callbacks.delete(id); },
    async invoke(command: string, args: any) {
      commands.push([command, command === 'plugin:event|listen' ? args.event : args]);
      if (command === 'plugin:event|listen') { if (args.event === 'echo-update-status') statusListener = args.handler; return args.handler; }
      if (command === 'plugin:event|unlisten') return;
      if (command === 'get_update_status') return { sequence: 1, status: { state: 'none' } };
      if (command === 'install_update') return installResponse;
      throw new Error(`Unexpected native command: ${command}`);
    }
  };
  (harness.window as any).__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
  const { view } = harness.mount() as { view: { motion(next: boolean): void } };
  const settle = async () => { for (let i = 0; i < 20; i++) await Promise.resolve(); harness.flush(); };
  await settle();
  const button = () => harness.target.querySelector('.dashboard-version button') as unknown as HTMLButtonElement | null;
  return {
    harness, commands, view, settle, button,
    version: () => harness.target.querySelector('.dashboard-version')!,
    announcement: () => harness.target.querySelector('.dashboard-version [role="status"]')?.textContent ?? null,
    emit(envelope: UpdateEnvelope) { callbacks.get(statusListener!)!({ payload: envelope }); harness.flush(); },
    respondToInstall(envelope: UpdateEnvelope) { installResponse = envelope; },
    async click() { button()!.click(); harness.settleEvents(); await settle(); },
    installs: () => commands.filter(([command]) => command === 'install_update')
  };
}

// Keep: the sidebar loop from offer to retry through the real Tauri event and invoke APIs.
test('sidebar update button walks available, downloading, waiting, installing and failed, and retries', async () => {
  const nav = await mountNav({ tauri: true });
  try {
    // Subscribe before reading, so a change between the two is never lost.
    expect(nav.commands.slice(0, 2)).toEqual([['plugin:event|listen', 'echo-update-status'], ['get_update_status', {}]]);
    expect(nav.button()).toBeNull();
    expect(nav.version().textContent!.trim()).toEndWith(`v${appVersion}`);
    expect(nav.version().classList.contains('has-update')).toBe(false);
    expect(nav.announcement()).toBe('');

    nav.emit({ sequence: 2, status: { state: 'available', version: '0.5.1' } });
    expect(nav.button()!.getAttribute('type')).toBe('button');
    expect(nav.button()!.getAttribute('aria-label')).toBe('Update Echo to v0.5.1');
    expect(nav.button()!.getAttribute('title')).toBe('Update Echo to v0.5.1');
    expect(nav.button()!.getAttribute('aria-disabled')).toBeNull();
    expect(nav.button()!.querySelector('path')!.getAttribute('d')).toBe('M12 4v11M7.5 10.5 12 15l4.5-4.5M5 19h14');
    expect(nav.version().classList.contains('has-update')).toBe(true);
    expect(nav.announcement()).toBe('');

    nav.respondToInstall({ sequence: 3, status: { state: 'downloading', version: '0.5.1', downloadedBytes: 0, totalBytes: null } });
    await nav.click();
    expect(nav.installs()).toEqual([['install_update', {}]]);
    expect(nav.button()!.getAttribute('aria-label')).toBe('Downloading Echo v0.5.1');
    expect(nav.button()!.getAttribute('aria-disabled')).toBe('true');
    expect(nav.button()!.querySelector('.ring-arc')!.getAttribute('stroke-dasharray')).toBe('25 100');
    expect(nav.announcement()).toBe('Downloading Echo v0.5.1');
    nav.view.motion(false); nav.harness.flush();
    expect(nav.button()!.querySelector('.ring-arc')!.classList.contains('spinning')).toBe(false);
    nav.view.motion(true); nav.harness.flush();
    expect(nav.button()!.querySelector('.ring-arc')!.classList.contains('spinning')).toBe(true);

    nav.emit({ sequence: 4, status: { state: 'downloading', version: '0.5.1', downloadedBytes: 512, totalBytes: 1024 } });
    expect(nav.button()!.getAttribute('aria-label')).toBe('Downloading Echo v0.5.1, 50%');
    expect(nav.button()!.querySelector('.ring-arc')!.getAttribute('stroke-dasharray')).toBe('50 100');
    expect(nav.button()!.querySelector('.ring-arc')!.classList.contains('spinning')).toBe(false);
    expect(nav.announcement()).toBe('Downloading Echo v0.5.1');

    // A busy button sends nothing, and stale or repeated sequences change nothing.
    await nav.click();
    expect(nav.installs()).toHaveLength(1);
    nav.emit({ sequence: 4, status: { state: 'available', version: '0.5.1' } });
    nav.emit({ sequence: 2, status: { state: 'failed', version: '0.5.1' } });
    expect(nav.button()!.getAttribute('aria-label')).toBe('Downloading Echo v0.5.1, 50%');

    nav.emit({ sequence: 5, status: { state: 'waiting_for_idle', version: '0.5.1' } });
    expect(nav.button()!.getAttribute('aria-label')).toBe('Echo v0.5.1 installs when this dictation ends');
    expect(nav.button()!.getAttribute('aria-disabled')).toBe('true');
    expect(nav.announcement()).toBe('Echo v0.5.1 installs when this dictation ends');

    nav.emit({ sequence: 6, status: { state: 'installing', version: '0.5.1' } });
    expect(nav.button()!.getAttribute('aria-label')).toBe('Installing Echo v0.5.1');
    expect(nav.button()!.getAttribute('aria-disabled')).toBe('true');
    expect(nav.button()!.querySelector('.ring-arc')!.getAttribute('stroke-dasharray')).toBe('100 100');

    nav.emit({ sequence: 7, status: { state: 'failed', version: '0.5.1' } });
    expect(nav.button()!.getAttribute('aria-label')).toBe('Update failed. Try again');
    expect(nav.button()!.getAttribute('title')).toBe('Update failed. Try again');
    expect(nav.button()!.getAttribute('aria-disabled')).toBeNull();
    expect(nav.button()!.classList.contains('failed')).toBe(true);
    expect(nav.announcement()).toBe('Update failed. Try again');

    nav.respondToInstall({ sequence: 8, status: { state: 'downloading', version: '0.5.1', downloadedBytes: 0, totalBytes: 2048 } });
    await nav.click();
    expect(nav.installs()).toHaveLength(2);
    expect(nav.button()!.getAttribute('aria-label')).toBe('Downloading Echo v0.5.1, 0%');

    // Whole percents rounded down, so the label never reads 100% before the download completes.
    nav.emit({ sequence: 9, status: { state: 'downloading', version: '0.5.1', downloadedBytes: 1, totalBytes: 3 } });
    expect(nav.button()!.getAttribute('aria-label')).toBe('Downloading Echo v0.5.1, 33%');
    nav.emit({ sequence: 10, status: { state: 'downloading', version: '0.5.1', downloadedBytes: 999, totalBytes: 1000 } });
    expect(nav.button()!.getAttribute('aria-label')).toBe('Downloading Echo v0.5.1, 99%');

    nav.emit({ sequence: 11, status: { state: 'none' } });
    expect(nav.button()).toBeNull();
    expect(nav.version().classList.contains('has-update')).toBe(false);

    await nav.harness.unmount();
    expect(nav.commands.at(-1)).toEqual(['plugin:event|unlisten', { event: 'echo-update-status', eventId: 1 }]);
  } finally { await nav.harness.cleanup(); }
});

// Keep: outside Tauri the navigation makes no update request, like BackgroundPanel's login-item read.
test('sidebar outside Tauri makes no update request and shows only the version', async () => {
  const nav = await mountNav({ tauri: false });
  try {
    expect(nav.commands).toEqual([]);
    expect(nav.button()).toBeNull();
    expect(nav.version().textContent!.trim()).toEndWith(`v${appVersion}`);
  } finally { await nav.harness.cleanup(); }
});

// Keep: a status read that resolves after a newer event cannot roll the view back.
test('controller drops a status read older than an event it already applied', async () => {
  let emit!: (envelope: UpdateEnvelope) => void, resolveRead!: (envelope: UpdateEnvelope) => void;
  const calls: string[] = [];
  const client: UpdateClient = {
    async listen(listener) { calls.push('listen'); emit = listener; return () => { calls.push('unlisten'); }; },
    status() { calls.push('status'); return new Promise((resolve) => { resolveRead = resolve; }); },
    async install() { throw new Error('not used'); }
  };
  const state: { view: UpdateView } = { view: { kind: 'hidden' } };
  const stop = createUpdateController(client, state).mount();
  for (let i = 0; i < 5; i++) await Promise.resolve();
  expect(calls).toEqual(['listen', 'status']);
  emit({ sequence: 5, status: { state: 'available', version: '0.5.1' } });
  resolveRead({ sequence: 4, status: { state: 'none' } });
  for (let i = 0; i < 5; i++) await Promise.resolve();
  expect(state.view).toEqual({ kind: 'available', version: '0.5.1' });
  stop();
  expect(calls).toEqual(['listen', 'status', 'unlisten']);
});

// Keep: a notify build's sidebar loop: the offer opens the download page through the same command,
// a failed browser open shows the address and announces it, and a retry sends the request again.
test('sidebar download button on a notify build opens the download page and shows the address when it cannot', async () => {
  const nav = await mountNav({ tauri: true });
  try {
    nav.emit({ sequence: 2, status: { state: 'download_page', version: '0.5.1' } });
    const offer = 'Download Echo v0.5.1';
    const instruction = 'Opens the download page in your browser; run the installer there to update';
    const description = () => nav.harness.target.querySelector('#update-description');
    expect(nav.button()!.getAttribute('aria-label')).toBe(offer);
    expect(nav.button()!.getAttribute('title')).toBe(`${offer}. ${instruction}`);
    expect(nav.button()!.getAttribute('aria-describedby')).toBe('update-description');
    expect(description()!.textContent).toBe(instruction);
    expect(nav.button()!.getAttribute('aria-disabled')).toBeNull();
    expect(nav.button()!.classList.contains('failed')).toBe(false);
    expect(nav.button()!.querySelector('path')!.getAttribute('d')).toBe('M12 4v11M7.5 10.5 12 15l4.5-4.5M5 19h14');
    expect(nav.button()!.querySelector('.ring-arc')).toBeNull();
    expect(nav.version().classList.contains('has-update')).toBe(true);
    expect(nav.announcement()).toBe('');

    nav.respondToInstall({ sequence: 3, status: { state: 'download_page_failed', version: '0.5.1' } });
    await nav.click();
    expect(nav.installs()).toEqual([['install_update', {}]]);
    const failed = 'Could not open your browser. Download Echo v0.5.1 from github.com/alegsm/Echo/releases';
    expect(nav.button()!.getAttribute('aria-label')).toBe(failed);
    expect(nav.button()!.getAttribute('title')).toBe(failed);
    expect(nav.button()!.getAttribute('aria-describedby')).toBeNull();
    expect(description()).toBeNull();
    expect(nav.button()!.getAttribute('aria-disabled')).toBeNull();
    expect(nav.button()!.classList.contains('failed')).toBe(true);
    expect(nav.button()!.querySelector('path')!.getAttribute('d')).toBe('M12 4v11M7.5 10.5 12 15l4.5-4.5M5 19h14');
    expect(nav.announcement()).toBe(failed);

    nav.respondToInstall({ sequence: 4, status: { state: 'download_page', version: '0.5.1' } });
    await nav.click();
    expect(nav.installs()).toHaveLength(2);
    expect(nav.button()!.getAttribute('aria-label')).toBe(offer);
    expect(description()!.textContent).toBe(instruction);
    expect(nav.button()!.classList.contains('failed')).toBe(false);
    expect(nav.announcement()).toBe('');

    nav.emit({ sequence: 5, status: { state: 'none' } });
    expect(nav.button()).toBeNull();
    expect(nav.version().classList.contains('has-update')).toBe(false);
  } finally { await nav.harness.cleanup(); }
});

// Keep: the notify views' copy and request rules, and that the install views keep theirs.
test('controller labels the download page views and requests from both', async () => {
  const page: UpdateView = { kind: 'download_page', version: '0.5.1' };
  const failed: UpdateView = { kind: 'download_page_failed', version: '0.5.1' };
  expect(updateLabel(page)).toBe('Download Echo v0.5.1');
  expect(updateDescription(page)).toBe('Opens the download page in your browser; run the installer there to update');
  expect(updateDescription(failed)).toBe('');
  expect(updateDescription({ kind: 'available', version: '0.5.1' })).toBe('');
  expect(updateLabel(failed)).toBe('Could not open your browser. Download Echo v0.5.1 from github.com/alegsm/Echo/releases');
  expect(updateAnnouncement(page)).toBe('');
  expect(updateAnnouncement(failed)).toBe(updateLabel(failed));
  expect(updateBusy(page)).toBe(false);
  expect(updateBusy(failed)).toBe(false);
  expect(updateLabel({ kind: 'available', version: '0.5.1' })).toBe('Update Echo to v0.5.1');
  expect(updateLabel({ kind: 'failed', version: '0.5.1' })).toBe('Update failed. Try again');

  const responses: UpdateEnvelope[] = [
    { sequence: 2, status: { state: 'download_page_failed', version: '0.5.1' } },
    { sequence: 3, status: { state: 'download_page', version: '0.5.1' } }
  ];
  let installs = 0;
  const client: UpdateClient = {
    async listen() { return () => {}; },
    async status() { return { sequence: 0, status: { state: 'none' } }; },
    async install() { installs++; return responses.shift()!; }
  };
  const state: { view: UpdateView } = { view: page };
  const controller = createUpdateController(client, state);
  await controller.install();
  expect(installs).toBe(1);
  expect(state.view).toEqual(failed);
  await controller.install();
  expect(installs).toBe(2);
  expect(state.view).toEqual(page);

  for (const view of [{ kind: 'hidden' }, { kind: 'waiting', version: '0.5.1' }, { kind: 'installing', version: '0.5.1' }] as UpdateView[]) {
    state.view = view;
    await controller.install();
  }
  expect(installs).toBe(2);
});

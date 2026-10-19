// tauri-driver hands the app its arguments but not an environment; the
// notes-e2e build turns `--s4-e2e-env=S4_E2E_KEY=value` into variables.
export function appArgs(env = {}) {
  const merged = {
    S4_E2E_STARTUP_ROUTE: 'notes',
    S4_E2E_BACKUP_DESTINATION: process.env.S4_E2E_BACKUP_DESTINATION,
    ...env,
  };
  return Object.entries(merged)
    .filter(([, value]) => value !== undefined && value !== '')
    .map(([key, value]) => {
      if (!key.startsWith('S4_E2E_')) throw new Error(`not a test variable: ${key}`);
      return `--s4-e2e-env=${key}=${value}`;
    });
}

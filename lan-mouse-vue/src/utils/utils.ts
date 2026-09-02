/** Range-check a port number. Returns null if valid, otherwise a
 *  human-readable error string. Used by both GeneralPanel (host port
 *  input) and ConnectionRow (per-client port input). */
export function validatePort(n: unknown): string | null {
  if (typeof n !== 'number' || !Number.isFinite(n) || n <= 0 || n > 65535)
    return 'port must be 1-65535'
  return null
}

/** Promise-based wrapper around the Clipboard API that never throws.
 *  Returns true on success, false if the browser denied access or
 *  the clipboard API is unavailable. */
export async function copyToClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    return false
  }
}

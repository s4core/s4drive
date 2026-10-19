/**
 * Request ids for Notes mutations.
 * The backend accepts only UUID v7 ids: they sort by creation time, which
 * keeps the local mutation journal ordered. crypto.randomUUID() is v4.
 */
export function newRequestId(now = Date.now(), bytes = crypto.getRandomValues(new Uint8Array(16))) {
  let millis = now;
  for (let i = 5; i >= 0; i -= 1) {
    bytes[i] = millis % 256;
    millis = Math.floor(millis / 256);
  }
  bytes[6] = (bytes[6] & 0x0f) | 0x70;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

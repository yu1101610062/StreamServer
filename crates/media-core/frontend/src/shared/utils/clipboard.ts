export async function copyText(value: string) {
  await navigator.clipboard.writeText(value);
}

export interface MergeTagOptions {
  max?: number;
  exclude?: string[];
}

/** 标签按原样 trim；分隔符不参与拆分，空格与逗号都可以是标签名的一部分。 */
export function normalizeTagName(raw: string): string | null {
  const trimmed = raw.trim();
  return trimmed ? trimmed : null;
}

/** 返回“当前 chip + 新增标签”的新数组；大小写不敏感去重，并应用排除项与上限。 */
export function mergeTagNames(
  current: string[],
  additions: string[],
  options: MergeTagOptions = {},
): string[] {
  const seen = new Set<string>();
  for (const name of options.exclude ?? []) {
    const normalized = normalizeTagName(name);
    if (normalized) seen.add(normalized.toLowerCase());
  }
  const next = [...current];
  for (const name of next) {
    const normalized = normalizeTagName(name);
    if (normalized) seen.add(normalized.toLowerCase());
  }
  for (const raw of additions) {
    const name = normalizeTagName(raw);
    if (!name) continue;
    const key = name.toLowerCase();
    if (seen.has(key)) continue;
    if (options.max != null && next.length >= options.max) break;
    seen.add(key);
    next.push(name);
  }
  return next;
}

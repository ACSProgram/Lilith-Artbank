export interface MergeTagOptions {
  max?: number;
  exclude?: string[];
}

export interface TagSuggestion {
  name: string;
  /** 已有该标签的图片数；不传则不显示计数。 */
  imageCount?: number;
}

/** 建议查询的触发规则：空输入立即查，非空去抖。 */
export function tagSuggestionRequest(text: string): {
  needle: string;
  debounceMs: number;
} {
  const needle = text.trim();
  return needle ? { needle, debounceMs: 160 } : { needle: "", debounceMs: 0 };
}

/** 过滤已选/排除项，大小写不敏感去重，按输入顺序取前 `limit` 个。 */
export function filterTagSuggestions(
  items: readonly TagSuggestion[],
  blocked: readonly string[],
  limit = 12,
): TagSuggestion[] {
  if (limit <= 0) return [];
  const blockedKeys = new Set<string>();
  for (const raw of blocked) {
    const name = normalizeTagName(raw);
    if (name) blockedKeys.add(name.toLowerCase());
  }
  const seen = new Set<string>();
  const next: TagSuggestion[] = [];
  for (const item of items) {
    const name = item.name.trim();
    if (!name) continue;
    const key = name.toLowerCase();
    if (blockedKeys.has(key) || seen.has(key)) continue;
    seen.add(key);
    next.push(item.imageCount == null ? { name } : { name, imageCount: item.imageCount });
    if (next.length >= limit) break;
  }
  return next;
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

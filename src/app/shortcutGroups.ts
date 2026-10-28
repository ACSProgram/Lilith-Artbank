/**
 * 快捷键总表的数据部分，与渲染分开，方便在 node 里直接测。
 *
 * 这里只写**真的生效**的键位。每一条都能对上具体实现：
 * - 网格/列表 → `LibraryView.handleKeyDown` / `handleItemClick`；
 * - 查看器 → `ImageViewer` 的全局 keydown；
 * - 应用 → `AppShell` 的全局 keydown。
 * 加键位时三条都要一起改，否则这张表会开始骗人。
 */

export interface ShortcutRow {
  keys: string[];
  action: string;
}

export interface ShortcutGroup {
  title: string;
  note?: string;
  rows: ShortcutRow[];
}

export const SHORTCUT_GROUPS: ShortcutGroup[] = [
  {
    title: "网格与列表",
    note: "焦点在图片区时生效；输入框里按键不触发。",
    rows: [
      { keys: ["↑", "↓", "←", "→"], action: "移动活动图（列表里上下为一行）" },
      { keys: ["Shift", "+ 方向键"], action: "从起点连选到新的活动图" },
      { keys: ["PageUp", "PageDown"], action: "整屏翻页" },
      { keys: ["Home", "End"], action: "回到开头 / 跳到末尾" },
      { keys: ["Enter"], action: "打开查看器" },
      { keys: ["Delete", "Backspace"], action: "移入回收站；在回收站里是永久删除" },
      { keys: ["Esc"], action: "先关浮层，再清除选择" },
      { keys: ["Ctrl", "A"], action: "全选当前结果" },
      { keys: ["Ctrl", "F"], action: "聚焦搜索框" },
      { keys: ["Ctrl", "Z"], action: "返回上一视图" },
      { keys: ["单击"], action: "选中这一张" },
      { keys: ["Ctrl", "+ 单击"], action: "加选 / 取消这一张" },
      { keys: ["Shift", "+ 单击"], action: "从上一张连选到这一张" },
    ],
  },
  {
    title: "查看器",
    rows: [
      { keys: ["←", "→"], action: "上一张 / 下一张" },
      { keys: ["Esc"], action: "依次退出裁剪、姿势、全屏，最后关闭查看器" },
      { keys: ["F"], action: "全屏切换" },
      { keys: ["0"], action: "适应窗口" },
      { keys: ["1"], action: "原始大小（100%）" },
      { keys: ["+", "-"], action: "放大 / 缩小" },
      { keys: ["Ctrl", "Z"], action: "撤销编辑" },
      { keys: ["Ctrl", "Y"], action: "重做编辑" },
      { keys: ["Ctrl", "Shift", "Z"], action: "重做编辑" },
    ],
  },
  {
    title: "应用",
    rows: [
      { keys: ["F11"], action: "窗口全屏" },
      { keys: ["Ctrl", "O"], action: "打开 / 切换图库" },
      { keys: ["Ctrl", ","], action: "打开设置" },
      { keys: ["?"], action: "打开这张快捷键总表" },
    ],
  },
];

/**
 * 同一组里重复出现的键位组合。
 *
 * 总表是手写的，改实现时很容易忘掉另一边；这里把它变成一个能跑的检查，
 * 而不是靠人眼比对。同一组内的重复才算冲突——`Esc` 在网格与查看器里各有一份是正常的。
 */
export function duplicateShortcutKeys(group: ShortcutGroup): string[] {
  const seen = new Set<string>();
  const duplicates = new Set<string>();
  for (const row of group.rows) {
    const combo = row.keys.join("+");
    if (seen.has(combo)) duplicates.add(combo);
    seen.add(combo);
  }
  return [...duplicates];
}

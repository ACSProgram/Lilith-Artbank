/** 虚拟网格/列表对外的滚动句柄，供键盘导航与 Home/End 复用。 */
export interface VirtualViewHandle {
  ensureIndex: (index: number) => void;
  scrollByPages: (pages: number) => void;
  scrollToTop: () => void;
  scrollToBottom: () => void;
}

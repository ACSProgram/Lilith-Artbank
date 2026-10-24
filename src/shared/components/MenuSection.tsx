import type { ReactNode } from "react";

interface MenuSectionProps {
  /** 分组小标题，小号大写灰字，不可点击。 */
  title: string;
  children: ReactNode;
}

/**
 * 菜单分组：替代“找相似 · xx”这类重复前缀。
 * 组与组之间自动画分隔线，第一组不画。
 */
export function MenuSection({ title, children }: MenuSectionProps) {
  return (
    <div className="menu-section">
      <div className="menu-section-title">
        <span>{title}</span>
      </div>
      {children}
    </div>
  );
}

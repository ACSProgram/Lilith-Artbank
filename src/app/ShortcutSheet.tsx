import { Modal } from "../shared/components/Modal";
import { Button } from "../shared/components/Button";
import { SHORTCUT_GROUPS } from "./shortcutGroups";

/**
 * 快捷键总表，只读。
 *
 * 数据在 `shortcutGroups.ts`，那里同时有一条「同组键位不许重复」的测试，
 * 防止手写总表与实现漂移。这里只管怎么显示。
 */
export function ShortcutSheet({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Modal
      open={open}
      title="快捷键"
      subtitle="只列出当前版本真正生效的键位；输入框里打字不会触发。"
      width={620}
      onClose={onClose}
      footer={
        <Button variant="primary" onClick={onClose}>
          知道了
        </Button>
      }
    >
      <div className="shortcut-sheet">
        {SHORTCUT_GROUPS.map((group) => (
          <section key={group.title} className="shortcut-group">
            <h3>{group.title}</h3>
            {group.note ? <p className="dialog-hint">{group.note}</p> : null}
            <dl className="shortcut-list">
              {group.rows.map((row) => (
                <div key={`${group.title}-${row.action}`} className="shortcut-row">
                  <dt>
                    {row.keys.map((key, index) => (
                      <span key={key}>
                        {index > 0 ? <span className="shortcut-plus">+</span> : null}
                        <kbd>{key}</kbd>
                      </span>
                    ))}
                  </dt>
                  <dd>{row.action}</dd>
                </div>
              ))}
            </dl>
          </section>
        ))}
      </div>
    </Modal>
  );
}

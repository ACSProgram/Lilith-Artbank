import { useEffect, useMemo, useState } from "react";
import { ChevronDown, ChevronRight, Folder, FolderOpen } from "lucide-react";
import { Button } from "./Button";
import { Modal } from "./Modal";

export interface FolderPickerItem {
  id: number;
  path: string;
}

interface FolderPickerDialogProps {
  open: boolean;
  title: string;
  subtitle?: string;
  folders: FolderPickerItem[];
  selectedPath?: string | null;
  /** 不允许选择的文件夹（例如移动文件夹时的自身与后代）。 */
  disabledIds?: Set<number>;
  allowRoot?: boolean;
  rootLabel?: string;
  onClose: () => void;
  onSelect: (folder: { id: number | null; path: string }) => void;
}

interface PickerNode extends FolderPickerItem {
  name: string;
  children: PickerNode[];
}

function buildNodes(folders: FolderPickerItem[]): PickerNode[] {
  const nodes = new Map<string, PickerNode>();
  for (const folder of folders) {
    if (!folder.path) continue;
    const name = folder.path.slice(folder.path.lastIndexOf("/") + 1);
    nodes.set(folder.path, { ...folder, name, children: [] });
  }
  const roots: PickerNode[] = [];
  for (const folder of folders) {
    if (!folder.path) continue;
    const node = nodes.get(folder.path);
    if (!node) continue;
    const index = folder.path.lastIndexOf("/");
    const parentPath = index < 0 ? "" : folder.path.slice(0, index);
    const parent = nodes.get(parentPath);
    if (parent) parent.children.push(node);
    else roots.push(node);
  }
  const sort = (items: PickerNode[]) => {
    items.sort((left, right) => left.name.localeCompare(right.name, "zh-Hans-CN"));
    for (const item of items) sort(item.children);
  };
  sort(roots);
  return roots;
}

export function FolderPickerDialog({
  open,
  title,
  subtitle,
  folders,
  selectedPath = null,
  disabledIds,
  allowRoot = true,
  rootLabel = "根目录",
  onClose,
  onSelect,
}: FolderPickerDialogProps) {
  const roots = useMemo(() => buildNodes(folders), [folders]);
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  const [chosen, setChosen] = useState<{ id: number | null; path: string }>({
    id: null,
    path: "",
  });

  useEffect(() => {
    if (!open) return;
    setChosen({ id: null, path: selectedPath ?? "" });
    const next = new Set<string>();
    let current = "";
    for (const part of (selectedPath ?? "").split("/")) {
      if (!part) continue;
      current = current ? `${current}/${part}` : part;
      next.add(current);
    }
    setExpanded(next);
  }, [open, selectedPath, folders]);

  const rootId = folders.find((folder) => folder.path === "")?.id ?? null;

  const renderNode = (node: PickerNode, depth: number) => {
    const active = chosen.path === node.path;
    const disabled = disabledIds?.has(node.id) ?? false;
    const hasChildren = node.children.length > 0;
    return (
      <div key={node.path}>
        <div
          className={`folder-picker-row${active ? " is-active" : ""}${
            disabled ? " is-disabled" : ""
          }`}
          style={{ paddingLeft: 8 + depth * 16 }}
        >
          <button
            type="button"
            className="folder-tree-toggle"
            aria-label={expanded.has(node.path) ? "折叠" : "展开"}
            disabled={!hasChildren}
            onClick={() => {
              setExpanded((current) => {
                const next = new Set(current);
                if (next.has(node.path)) next.delete(node.path);
                else next.add(node.path);
                return next;
              });
            }}
          >
            {hasChildren ? (
              expanded.has(node.path) ? (
                <ChevronDown size={13} />
              ) : (
                <ChevronRight size={13} />
              )
            ) : (
              <span className="folder-tree-spacer" />
            )}
          </button>
          <button
            type="button"
            className="folder-picker-label"
            disabled={disabled}
            title={node.path}
            onClick={() => setChosen({ id: node.id, path: node.path })}
          >
            {active ? <FolderOpen size={14} /> : <Folder size={14} />}
            <span>{node.name}</span>
          </button>
        </div>
        {hasChildren && expanded.has(node.path)
          ? node.children.map((child) => renderNode(child, depth + 1))
          : null}
      </div>
    );
  };

  return (
    <Modal
      open={open}
      title={title}
      subtitle={subtitle}
      onClose={onClose}
      width={480}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            取消
          </Button>
          <Button variant="primary" onClick={() => onSelect(chosen)}>
            选择
          </Button>
        </>
      }
    >
      <div className="folder-picker">
        {allowRoot ? (
          <button
            type="button"
            className={`folder-tree-row${chosen.path === "" ? " is-selected" : ""}`}
            onClick={() => setChosen({ id: rootId, path: "" })}
          >
            <span className="folder-tree-toggle">
              <span className="folder-tree-spacer" />
            </span>
            <FolderOpen size={14} />
            <span className="folder-tree-name">{rootLabel}</span>
          </button>
        ) : null}
        {roots.map((node) => renderNode(node, 0))}
        {roots.length === 0 ? <p className="form-note">还没有文件夹。</p> : null}
      </div>
    </Modal>
  );
}

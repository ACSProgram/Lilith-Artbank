import { useEffect, useState } from "react";
import { ScanSearch, Layers, Plus } from "lucide-react";
import { Button } from "../shared/components/Button";
import { Modal } from "../shared/components/Modal";
import { aiApi } from "../modules/ai/api";
import { libraryApi } from "../modules/library/api";
import type { SimilarityGroupsResult } from "../modules/ai/types";
import type { SimilarGroupKind } from "../shared/similarity";
import { SIMILAR_GROUP_LABELS } from "../shared/similarity";
import { errorMessage } from "../shared/errors";
import { useNotices } from "./noticeContext";

interface SimilarityGroupsDialogProps {
  open: boolean;
  onClose: () => void;
  onChanged: () => void;
}

const VISIBLE_GROUPS = 200;

export function SimilarityGroupsDialog({ open, onClose, onChanged }: SimilarityGroupsDialogProps) {
  const { notify } = useNotices();
  const [kind, setKind] = useState<SimilarGroupKind>("phash");
  const [phashThreshold, setPhashThreshold] = useState("6");
  const [similarityThreshold, setSimilarityThreshold] = useState("0.97");
  const [baseName, setBaseName] = useState("相似组");
  const [busy, setBusy] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<SimilarityGroupsResult | null>(null);

  useEffect(() => {
    if (open) return;
    setBusy(false);
    setSaving(false);
    setError(null);
  }, [open]);

  const scan = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const next = await aiApi.findSimilarGroups(
        kind,
        kind === "phash" ? Number(phashThreshold) : undefined,
        kind === "embedding" ? Number(similarityThreshold) : undefined,
      );
      setResult(next);
    } catch (cause) {
      setError(errorMessage(cause));
      setResult(null);
    } finally {
      setBusy(false);
    }
  };

  const createCollection = async (name: string, ids: number[]) => {
    const collection = await libraryApi.createCollection(name);
    await libraryApi.addImagesToCollection(collection.id, ids);
  };

  const uniqueName = (used: Set<string>, base: string, index: number) => {
    const prefix = base.trim() || "相似组";
    let name = `${prefix} ${index + 1}`;
    let suffix = 2;
    while (used.has(name.toLowerCase())) {
      name = `${prefix} ${index + 1}-${suffix}`;
      suffix += 1;
    }
    used.add(name.toLowerCase());
    return name;
  };

  const saveGroups = async (groups: number[][]) => {
    if (saving || groups.length === 0) return;
    if (groups.length > 100) {
      notify("一次最多创建 100 个集合", "error", "可以改用“合并保存”，或分批保存。");
      return;
    }
    setSaving(true);
    try {
      const collections = await libraryApi.getCollections();
      const used = new Set(collections.map((collection) => collection.name.toLowerCase()));
      for (let index = 0; index < groups.length; index += 1) {
        const name = uniqueName(used, baseName, index);
        await createCollection(name, groups[index]);
      }
      notify(`已创建 ${groups.length} 个集合`, "success");
      onChanged();
    } catch (cause) {
      notify("保存相似组失败", "error", errorMessage(cause));
    } finally {
      setSaving(false);
    }
  };

  const mergeGroups = async () => {
    if (saving || !result || result.groups.length === 0) return;
    setSaving(true);
    try {
      const ids = [...new Set(result.groups.flat())];
      const collections = await libraryApi.getCollections();
      const used = new Set(collections.map((collection) => collection.name.toLowerCase()));
      const name = uniqueName(used, baseName, 0);
      await createCollection(name, ids);
      notify(`已把 ${ids.length.toLocaleString()} 张图片保存为集合`, "success");
      onChanged();
    } catch (cause) {
      notify("保存相似组失败", "error", errorMessage(cause));
    } finally {
      setSaving(false);
    }
  };

  const visible = result?.groups.slice(0, VISIBLE_GROUPS) ?? [];

  return (
    <Modal
      open={open}
      title="相似分组"
      subtitle="pHash 用 8×32 位分段索引找候选；embedding 模式先用 pHash 候选，再用中心化余弦复核，避免全库 N² 比较。"
      width={760}
      onClose={onClose}
      footer={
        <Button variant="primary" onClick={onClose}>
          完成
        </Button>
      }
    >
      <section className="dialog-section">
        <h3>
          <ScanSearch size={14} /> 扫描范围
        </h3>
        <div className="dialog-inline">
          <select
            className="input"
            value={kind}
            disabled={busy}
            onChange={(event) => setKind(event.target.value as SimilarGroupKind)}
          >
            <option value="phash">pHash 相似组</option>
            <option value="embedding">embedding 复核组</option>
          </select>
          {kind === "phash" ? (
            <label className="dialog-field">
              Hamming 阈值
              <input
                className="input"
                type="number"
                min={0}
                max={7}
                value={phashThreshold}
                disabled={busy}
                onChange={(event) => setPhashThreshold(event.target.value)}
              />
            </label>
          ) : (
            <label className="dialog-field">
              余弦阈值
              <input
                className="input"
                type="number"
                min={0.5}
                max={1}
                step={0.01}
                value={similarityThreshold}
                disabled={busy}
                onChange={(event) => setSimilarityThreshold(event.target.value)}
              />
            </label>
          )}
          <Button
            size="sm"
            variant="primary"
            icon={<ScanSearch size={13} />}
            loading={busy}
            onClick={() => void scan()}
          >
            扫描
          </Button>
        </div>
        <p className="dialog-hint">
          {kind === "phash"
            ? "阈值 0-7 时索引保证不漏候选；超过 7 请使用单张“找相似”。"
            : "只在 pHash 距离不超过 7 的候选内复核；没有 embedding 的图片会计入缺失。"}
        </p>
        {error ? <p className="dialog-error">{error}</p> : null}
      </section>

      {result ? (
        <section className="dialog-section">
          <h3>
            <Layers size={14} /> 结果
          </h3>
          <div className="dialog-inline">
            <span className="dialog-hint">
              {SIMILAR_GROUP_LABELS[result.kind]} · {result.groups.length.toLocaleString()} 组 ·{" "}
              {result.elapsedMs}ms
              {result.missing > 0 ? ` · ${result.missing.toLocaleString()} 张缺少数据` : ""}
              {result.truncated ? " · 已截断" : ""}
            </span>
            <input
              className="input"
              value={baseName}
              maxLength={60}
              disabled={saving}
              onChange={(event) => setBaseName(event.target.value)}
              aria-label="集合名称前缀"
            />
            <Button
              size="sm"
              variant="secondary"
              icon={<Plus size={13} />}
              loading={saving}
              disabled={result.groups.length === 0}
              onClick={() => void mergeGroups()}
            >
              合并保存
            </Button>
            <Button
              size="sm"
              variant="secondary"
              icon={<Layers size={13} />}
              loading={saving}
              disabled={result.groups.length === 0 || result.groups.length > 100}
              title={
                result.groups.length > 100
                  ? "一次最多创建 100 个集合，请改用合并保存"
                  : undefined
              }
              onClick={() => void saveGroups(result.groups)}
            >
              每组一个集合
            </Button>
          </div>
          {visible.length === 0 ? (
            <p className="dialog-hint">没有找到重复或相似组。</p>
          ) : (
            <div className="similarity-group-list">
              {visible.map((ids, index) => (
                <div className="similarity-group-row" key={`${index}-${ids[0]}`}>
                  <span className="similarity-group-index">第 {index + 1} 组</span>
                  <span className="similarity-group-count">{ids.length.toLocaleString()} 张</span>
                  <span className="similarity-group-ids">
                    {ids.slice(0, 12).map((id) => `#${id}`).join("、")}
                    {ids.length > 12 ? " …" : ""}
                  </span>
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={saving}
                    onClick={() => void saveGroups([ids])}
                  >
                    存为集合
                  </Button>
                </div>
              ))}
            </div>
          )}
          {result.groups.length > VISIBLE_GROUPS ? (
            <p className="dialog-hint">
              仅显示前 {VISIBLE_GROUPS} 组；批量按钮会处理全部 {result.groups.length.toLocaleString()} 组。
            </p>
          ) : null}
        </section>
      ) : null}
    </Modal>
  );
}

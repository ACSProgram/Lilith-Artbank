import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { AlertCircle, CheckCircle2, FolderOpen, Library } from "lucide-react";
import { Button } from "../shared/components/Button";
import { Field } from "../shared/components/Field";
import { Modal } from "../shared/components/Modal";
import { Spinner } from "../shared/components/Spinner";
import { appApi, browserPreview } from "./api";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";
import type { LibraryInspection } from "./types";

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "未知错误";
}

function basename(path: string): string {
  const trimmed = path.trim().replace(/[\\/]+$/, "");
  const parts = trimmed.split(/[\\/]/);
  return parts[parts.length - 1] || "";
}

export function LibraryDialog({ open: isOpen, onClose }: { open: boolean; onClose: () => void }) {
  const { addLibrary } = useAppRuntime();
  const { notify } = useNotices();
  const [name, setName] = useState("");
  const [path, setPath] = useState("");
  const [inspection, setInspection] = useState<LibraryInspection | null>(null);
  const [inspecting, setInspecting] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!isOpen) return;
    setName("");
    setPath("");
    setInspection(null);
    setError(null);
    setInspecting(false);
    setSubmitting(false);
  }, [isOpen]);

  useEffect(() => {
    if (!isOpen) return;
    const trimmed = path.trim();
    if (!trimmed) {
      setInspection(null);
      setInspecting(false);
      return;
    }
    let cancelled = false;
    setInspecting(true);
    const timer = window.setTimeout(() => {
      void appApi
        .inspectLibraryPath(trimmed)
        .then((result) => {
          if (cancelled) return;
          setInspection(result);
          if (result.kind === "library") {
            setName((current) => current.trim() || result.manifest.name);
          } else if (result.kind === "empty") {
            setName((current) => current.trim() || basename(trimmed));
          }
        })
        .catch((cause) => {
          if (!cancelled) {
            setInspection({ kind: "invalid", reason: errorMessage(cause) });
          }
        })
        .finally(() => {
          if (!cancelled) setInspecting(false);
        });
    }, 250);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [path, isOpen]);

  const canSubmit = useMemo(() => {
    if (!path.trim() || inspecting || submitting) return false;
    if (!inspection) return false;
    if (inspection.kind === "library") return true;
    if (inspection.kind === "empty") return name.trim().length > 0;
    return false;
  }, [path, inspecting, submitting, inspection, name]);

  const pickFolder = async () => {
    if (browserPreview) {
      setPath("G:\\Lilith Image Library\\演示图库");
      return;
    }
    try {
      const selected = await open({
        directory: true,
        multiple: false,
        title: "选择图库目录",
      });
      if (typeof selected === "string") setPath(selected);
    } catch (cause) {
      notify("选择目录失败", "error", errorMessage(cause));
    }
  };

  const submit = async () => {
    if (!canSubmit) return;
    setSubmitting(true);
    setError(null);
    try {
      const existing = inspection?.kind === "library";
      await addLibrary(existing ? "" : name.trim(), path.trim());
      onClose();
    } catch (cause) {
      const message = errorMessage(cause);
      setError(message);
      notify("打开图库失败", "error", message);
    } finally {
      setSubmitting(false);
    }
  };

  const actionLabel =
    inspection?.kind === "library" ? "打开图库" : inspection?.kind === "empty" ? "创建图库" : "打开或创建";

  return (
    <Modal
      open={isOpen}
      title="打开或创建图库"
      subtitle="图库目录保存图片本体与 .artbank 数据。已有图库会直接打开，空目录会初始化新库。"
      onClose={onClose}
      width={620}
      dismissible={!submitting}
      footer={
        <>
          <Button variant="ghost" onClick={onClose} disabled={submitting}>
            取消
          </Button>
          <Button
            variant="primary"
            onClick={() => void submit()}
            disabled={!canSubmit}
            loading={submitting}
          >
            {actionLabel}
          </Button>
        </>
      }
    >
      <div className="dialog-form">
        <Field label="图库目录" hint="支持带空格和中文的绝对路径。">
          <div className="input-row">
            <input
              className="input"
              value={path}
              onChange={(event) => setPath(event.target.value)}
              placeholder={"例如 G:\\Lilith Image Library\\参考"}
              spellCheck={false}
              autoFocus
            />
            <Button variant="secondary" icon={<FolderOpen size={16} />} onClick={() => void pickFolder()}>
              浏览
            </Button>
          </div>
        </Field>

        <div className={`inspection ${inspection?.kind ?? "unknown"}`}>
          {inspecting ? (
            <>
              <Spinner size={16} />
              <span>正在检查目录…</span>
            </>
          ) : inspection?.kind === "library" ? (
            <>
              <CheckCircle2 size={18} />
              <div>
                <strong>已有图库：{inspection.manifest.name}</strong>
                <span>格式 v{inspection.manifest.formatVersion} · 将直接打开</span>
              </div>
            </>
          ) : inspection?.kind === "empty" ? (
            <>
              <Library size={18} />
              <div>
                <strong>空目录</strong>
                <span>将在目录内创建 .artbank、SQLite 元数据库与缩略图包目录。</span>
              </div>
            </>
          ) : inspection?.kind === "invalid" ? (
            <>
              <AlertCircle size={18} />
              <div>
                <strong>无法使用该目录</strong>
                <span>{inspection.reason}</span>
              </div>
            </>
          ) : (
            <>
              <FolderOpen size={18} />
              <span>选择或输入一个目录。</span>
            </>
          )}
        </div>

        {inspection?.kind !== "library" ? (
          <Field label="图库名称" hint="只影响列表与窗口标题，不会重命名目录。">
            <input
              className="input"
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder="例如 参考"
              maxLength={120}
            />
          </Field>
        ) : null}

        {error ? <div className="form-error">{error}</div> : null}
        {browserPreview ? (
          <p className="form-note">浏览器预览模式：点击“浏览”会填入演示路径，不会访问磁盘。</p>
        ) : null}
      </div>
    </Modal>
  );
}

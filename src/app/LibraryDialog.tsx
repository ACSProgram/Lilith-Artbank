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
import { errorMessage } from "../shared/errors";

function basename(path: string): string {
  const trimmed = path.trim().replace(/[\\/]+$/, "");
  const parts = trimmed.split(/[\\/]/);
  return parts[parts.length - 1] || "";
}

export function LibraryDialog({ open: isOpen, onClose }: { open: boolean; onClose: () => void }) {
  const { current, createLibrary, openLibrary } = useAppRuntime();
  const { notify, confirm } = useNotices();
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
    // 已有图库时是“换库”：前端按 URL 缓存缩略图、按库构造模块级状态，热切换要逐项清理
    // 才不会串库，重启是唯一可靠的复位方式，所以先确认再换。
    const switching = current != null;
    if (switching) {
      const accepted = await confirm({
        title: "更换图库会重启应用",
        message: "当前图库会先关闭，应用随后自动重启并打开新图库。",
        confirmLabel: "重启并打开",
      });
      if (!accepted) return;
    }
    setSubmitting(true);
    setError(null);
    try {
      if (inspection?.kind === "library") {
        await openLibrary(path.trim());
      } else {
        await createLibrary(name.trim(), path.trim());
      }
      if (switching) {
        try {
          await appApi.restartApp();
        } catch (cause) {
          // 图库已经开好了，失败的是重启这一步，不能报成“打开图库失败”。
          const message = `图库已打开，但自动重启失败：${errorMessage(cause)}`;
          setError(message);
          notify("自动重启失败", "error", "新图库已打开，请手动重启应用完成切换。");
          return;
        }
        if (browserPreview) onClose();
        return;
      }
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
      title={current ? "更换图库" : "打开或创建图库"}
      subtitle={
        current
          ? "新图库会替换当前打开的图库，替换后应用自动重启；当前图库的数据不会被修改。"
          : "图库目录里放图片和图库数据。已有图库会直接打开，空目录会初始化新库。"
      }
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
        <Field label="图库目录" plain>
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
                <span>将直接打开</span>
              </div>
            </>
          ) : inspection?.kind === "empty" ? (
            <>
              <Library size={18} />
              <div>
                <strong>空目录</strong>
                <span>将在这里创建图库数据，目录里已有的图片不会被改动。</span>
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
          <Field label="图库名称" hint="只影响显示名称，不会重命名目录。">
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

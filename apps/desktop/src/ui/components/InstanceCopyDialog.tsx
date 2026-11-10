import { useEffect, useRef, useState } from "react";
import { readShellSnapshot, requestCancel } from "../../bridge";
import type { InstalledGame } from "../views/game-install-api";
import { gameInstallApi } from "../views/game-install-api";
import { encodeInstanceReference, instanceCopyApi, instanceCopyError, parseInstanceReference, type AcceptedCopy } from "../views/instance-copy-api";
import { Button } from "./Button";
import { Dialog } from "./Dialog";

export interface InstanceCopyDialogProps {
  open: boolean;
  source: InstalledGame | null;
  games: InstalledGame[];
  onClose: () => void;
  onComplete: () => void;
  onBusyChange?: (busy: boolean) => void;
}

/** Only the core task's terminal result can announce a created instance. */
export function InstanceCopyDialog({ open, source, games, onClose, onComplete, onBusyChange }: InstanceCopyDialogProps) {
  const [selected, setSelected] = useState<InstalledGame | null>(source);
  const [name, setName] = useState("");
  const [includeSaves, setIncludeSaves] = useState(false);
  const [reference, setReference] = useState("");
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");
  const [pending, setPending] = useState(false);
  const [active, setActive] = useState<AcceptedCopy | null>(null);
  const [cancelPending, setCancelPending] = useState(false);
  const busy = useRef(false);
  const mounted = useRef(true);
  const onCompleteRef = useRef(onComplete);
  onCompleteRef.current = onComplete;
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  useEffect(() => { onBusyChange?.(pending); }, [pending, onBusyChange]);

  useEffect(() => {
    if (!open || busy.current) return;
    setSelected(source); setName(source ? `${source.name} 副本` : ""); setIncludeSaves(false);
    setReference(""); setMessage(""); setError("");
  }, [open, source]);

  useEffect(() => {
    if (!active) return;
    let disposed = false;
    let missingReads = 0;
    let timer: number | undefined;
    async function observe() {
      try {
        const snapshot = await readShellSnapshot();
        if (disposed) return;
        const task = snapshot.tasks.find((item) => item.id === active!.taskId && item.operationId === active!.operationId);
        if (!task) {
          setError("暂时找不到复制任务，正在重新读取。");
          if (++missingReads >= 3) {
            const instances = await gameInstallApi().instances();
            if (disposed) return;
            busy.current = false; setPending(false); setActive(null); setCancelPending(false);
            if (instances.some((game) => game.id === active!.newInstanceId)) {
              setError(""); setMessage("副本已创建。"); onCompleteRef.current();
            } else setError("未能恢复复制进度，请刷新实例列表后重试。");
            return;
          }
        } else if (["succeeded", "failed", "cancelled"].includes(task.state)) {
          setError("");
          busy.current = false; setPending(false); setActive(null); setCancelPending(false);
          if (task.state === "succeeded") { setMessage("副本已创建。"); onCompleteRef.current(); }
          else if (task.state === "cancelled") setMessage("已取消复制。");
          else setError(instanceCopyError(task.error));
          return;
        } else { missingReads = 0; setError(""); }
      } catch {
        if (disposed) return;
        setError("连接暂时中断，正在重新读取复制进度。");
      }
      if (!disposed) timer = window.setTimeout(() => { void observe(); }, 500);
    }
    void observe();
    return () => { disposed = true; window.clearTimeout(timer); };
  }, [active]);

  function resolveReference(text: string) {
    setReference(text); setError(""); setMessage("");
    try {
      const id = parseInstanceReference(text);
      const game = games.find((item) => item.id === id);
      if (!game) { setSelected(null); setError("这份引用不在当前实例库中。跨设备请使用整合包导入。"); return; }
      setSelected(game); setName(`${game.name} 副本`);
    } catch { setSelected(null); setError("请粘贴从实例页复制的引用。"); }
  }

  async function readClipboard() {
    try { resolveReference(await navigator.clipboard.readText()); }
    catch { setMessage("请在下方粘贴实例引用。"); }
  }

  async function copyReference() {
    if (!selected) return;
    const text = encodeInstanceReference(selected.id);
    setReference(text); setError("");
    try { await navigator.clipboard.writeText(text); setMessage("实例引用已复制，可在实例页粘贴。"); }
    catch { setMessage("选中下方的实例引用并复制。"); }
  }

  async function createCopy() {
    if (!selected || busy.current) return;
    busy.current = true; setPending(true); setError(""); setMessage("");
    try {
      const accepted = await instanceCopyApi().copy(selected.id, `copy-${crypto.randomUUID()}`, name, includeSaves);
      if (mounted.current) setActive(accepted);
    } catch (failure) {
      busy.current = false;
      if (mounted.current) { setPending(false); setError(instanceCopyError(failure)); }
    }
  }

  async function cancelCopy() {
    if (!active || cancelPending) return;
    setCancelPending(true);
    try {
      const snapshot = await readShellSnapshot();
      const task = snapshot.tasks.find((item) => item.id === active.taskId && item.operationId === active.operationId);
      if (task) await requestCancel(task.id, task.attemptGeneration);
      else setCancelPending(false);
    } catch { setError("未能取消，请重试。"); setCancelPending(false); }
  }

  return <Dialog open={open} title={source ? "复制实例" : "粘贴实例"} onClose={onClose}
    actions={<><Button onClick={onClose}>关闭</Button>{pending
      ? <Button disabled={!active || cancelPending} onClick={() => void cancelCopy()}>{cancelPending ? "正在取消" : "取消复制"}</Button>
      : <Button variant="primary" disabled={!selected || !name.trim() || Boolean(message === "副本已创建。")} onClick={() => void createCopy()}>创建副本</Button>}</>}>
    {selected ? <p>原实例：{selected.name} · {selected.version.gameVersion}</p> : <p>粘贴同一个实例库中的实例引用，创建独立副本。</p>}
    {!source ? <Button size="sm" disabled={pending} onClick={() => void readClipboard()}>读取剪贴板</Button> : null}
    {selected ? <>
      <label className="install-field">副本名称<input maxLength={128} value={name} disabled={pending} onChange={(event) => setName(event.target.value)} /></label>
      <label className="copy-save-choice"><input type="checkbox" checked={includeSaves} disabled={pending} onChange={(event) => setIncludeSaves(event.target.checked)} /> 同时复制存档</label>
      <p>游戏文件、模组与设置会复制到新目录。</p>
      <Button size="sm" disabled={pending} onClick={() => void copyReference()}>复制实例引用</Button>
    </> : null}
    {!source || reference ? <label className="install-field">实例引用<textarea aria-label="实例引用" rows={3} value={reference} disabled={pending}
      onChange={(event) => resolveReference(event.target.value)} /></label> : null}
    {pending ? <p role="status">{cancelPending ? "正在结束复制…" : "正在复制游戏文件…"}</p> : null}
    {message ? <p role="status">{message}</p> : null}
    {error ? <p role="alert">{error}</p> : null}
  </Dialog>;
}

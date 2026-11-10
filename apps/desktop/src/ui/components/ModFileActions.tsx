import { useEffect, useRef, useState } from "react";
import { modToggleApi, modToggleError, type ModFileState } from "../views/mod-toggle-api";
import { Button } from "./Button";

export function ModFileActions({ instanceId, fileName, bytes, enabled, onCommitted, onBusyChange }: {
  instanceId: string; fileName: string; bytes: number; enabled: boolean;
  onCommitted: (file: ModFileState) => void; onBusyChange: (busy: boolean) => void;
}) {
  const [file, setFile] = useState<ModFileState | null>(null);
  const [phase, setPhase] = useState<"loading" | "ready" | "saving" | "blocked">("loading");
  const [message, setMessage] = useState("");
  const [revision, setRevision] = useState(0);
  const mounted = useRef(false);
  const busyCallback = useRef(onBusyChange); busyCallback.current = onBusyChange;
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; busyCallback.current(false); };
  }, []);
  useEffect(() => {
    let active = true;
    setPhase("loading"); setMessage("");
    void modToggleApi().snapshot(instanceId, fileName).then(value => {
      if (!active) return;
      if (value.bytes !== bytes || value.enabled !== enabled) { setPhase("blocked"); setMessage("文件已变化，请刷新清单后重新选择。"); return; }
      setFile(value); setPhase("ready");
    }).catch(() => { if (active) { setPhase("blocked"); setMessage("无法读取文件状态；当前仅支持不超过 32 MiB 的常规文件。"); } });
    return () => { active = false; };
  }, [instanceId, fileName, bytes, enabled, revision]);
  async function toggle() {
    if (phase !== "ready" || file?.state !== "ready") return;
    setPhase("saving"); setMessage(""); busyCallback.current(true);
    try {
      const saved = await modToggleApi().toggle(file);
      if (mounted.current) { setPhase("blocked"); onCommitted(saved); }
    } catch (error) {
      if (mounted.current) { setPhase("blocked"); setMessage(modToggleError(error)); }
    } finally { if (mounted.current) busyCallback.current(false); }
  }
  return <div className="mod-file-actions">
    <div className="mod-file-actions__buttons">
      <Button size="sm" pending={phase === "saving"} pendingLabel="正在处理" disabled={phase !== "ready" || file?.state !== "ready"} onClick={() => void toggle()}>{enabled ? "禁用此模组" : "启用此模组"}</Button>
      <Button size="sm" disabled={phase === "loading" || phase === "saving"} onClick={() => setRevision(value => value + 1)}>重新读取状态</Button>
    </div>
    {phase === "loading" ? <p role="status">正在核对文件状态…</p> : null}
    {phase === "ready" && file?.state !== "ready" ? <p role="status">实例当前不可修改，请结束游戏或任务后重新读取状态。</p> : null}
    {message ? <p role="alert">{message}</p> : null}
    <p className="settings-note">启用／禁用通过切换文件后缀实现，不代表已验证模组兼容性。</p>
  </div>;
}

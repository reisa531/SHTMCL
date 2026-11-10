import { useEffect, useRef, useState } from "react";
import { instanceSettingsApi, instanceSettingsError, type InstanceSettingsSnapshot } from "../views/instance-settings-api";
import { Button } from "./Button";

/** Name writes use the instance owner's revision/lease checks and are read back. */
export function InstanceNameSettings({ instanceId, onRefresh }: { instanceId: string; onRefresh?: () => void }) {
  const [current, setCurrent] = useState<InstanceSettingsSnapshot | null>(null);
  const [name, setName] = useState("");
  const [phase, setPhase] = useState<"loading" | "ready" | "saving" | "blocked">("loading");
  const [message, setMessage] = useState("");
  const [reload, setReload] = useState(0);
  const mounted = useRef(false);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  useEffect(() => {
    let active = true;
    setPhase("loading"); setMessage("");
    void instanceSettingsApi().read(instanceId).then((value) => {
      if (!active) return;
      setCurrent(value); setName(value.name); setPhase("ready");
    }).catch(() => {
      if (active) { setPhase("blocked"); setMessage("无法读取实例设置，请重试。"); }
    });
    return () => { active = false; };
  }, [instanceId, reload]);

  const pending = phase === "loading" || phase === "saving";
  const writable = phase === "ready" && current?.state === "ready";
  const trimmed = name.trim();
  const valid = Boolean(trimmed) && trimmed.length <= 200 && !/[\u0000-\u001f\u007f]/.test(trimmed);
  const save = async () => {
    if (!writable || !valid || !current || trimmed === current.name) return;
    setPhase("saving"); setMessage("");
    try {
      await instanceSettingsApi().rename(instanceId, current.revision, trimmed);
      onRefresh?.();
      const saved = await instanceSettingsApi().read(instanceId);
      if (!mounted.current) return;
      setCurrent(saved); setName(saved.name); setPhase("ready");
      setMessage(saved.name === trimmed ? "名称已保存。" : "名称已被其他操作更新，已显示最新结果。");
    } catch (error) {
      if (mounted.current) { setPhase("blocked"); setMessage(instanceSettingsError(error)); }
    }
  };

  return <form className="instance-name-settings" aria-label="实例名称设置" onSubmit={(event) => { event.preventDefault(); void save(); }}>
    <label className="install-field">实例名称<input value={name} maxLength={200} disabled={!writable}
      onChange={(event) => { setName(event.target.value); setMessage(""); }} /></label>
    <div className="instance-name-settings__actions">
      <Button type="submit" size="sm" variant="primary" pending={phase === "saving"} pendingLabel="正在保存"
        disabled={!writable || !valid || trimmed === current?.name}>保存名称</Button>
      <Button size="sm" disabled={pending} onClick={() => setReload((value) => value + 1)}>重新读取</Button>
    </div>
    {pending && phase === "loading" ? <p role="status">正在读取实例设置…</p> : null}
    {phase === "ready" && current?.state !== "ready" ? <p role="status">实例当前不可修改，请结束游戏或任务后重新读取。</p> : null}
    {message ? <p role={phase === "blocked" ? "alert" : "status"}>{message}</p> : null}
    <p className="settings-note">修改名称不会移动实例目录；重新读取会恢复为已保存的名称。</p>
  </form>;
}

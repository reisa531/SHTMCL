import { useRef, useState, type FormEvent } from "react";
import { Button } from "../components/Button";
import { Panel } from "../components/Panel";
import type { MotionPoint } from "../components/motion";
import { gameInstallApi, installError } from "./game-install-api";
import type { CachedEnvironment } from "./environment-api";

export function CreateFromEnvironment({ environment, onAccepted }: { environment: CachedEnvironment; onAccepted: (origin?: MotionPoint) => void }) {
  const [name, setName] = useState(`Minecraft ${environment.versionId}`);
  const [pending, setPending] = useState(false), [error, setError] = useState("");
  const lock = useRef(false);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (lock.current || !name.trim()) return;
    const rect = event.currentTarget.querySelector('button[type="submit"]')?.getBoundingClientRect();
    const origin = rect ? { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 } : undefined;
    lock.current = true; setPending(true); setError("");
    try { await gameInstallApi().install(environment.versionId, `game-${crypto.randomUUID()}`, name, environment.id); onAccepted(origin); }
    catch (error) { setError(installError(error)); }
    finally { lock.current = false; setPending(false); }
  }
  return <Panel title={`Minecraft ${environment.versionId} · 原版`}>
    <p>使用已下载的环境创建独立实例；创建时校验缓存并补齐缺失文件。</p>
    <form className="install-submit" onSubmit={(event) => void submit(event)}>
      <label className="install-field">实例名称<input required maxLength={80} value={name} onChange={(event) => setName(event.target.value)} /></label>
      <Button type="submit" variant="primary" pending={pending} disabled={!name.trim()} pendingLabel="正在提交">创建实例</Button>
    </form>
    {error ? <p role="alert" className="install-message">{error}</p> : null}
    <p className="field-note">存档和设置单独保存，Java和账户在启动时准备。</p>
  </Panel>;
}

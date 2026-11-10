import { useEffect, useId, useRef, useState } from "react";
import { isTauriRuntime } from "../../bridge";
import { Button } from "../components/Button";
import { Dialog } from "../components/Dialog";
import type { MotionPoint } from "../components/motion";
import { EmptyState } from "../components/EmptyState";
import { InlineError } from "../components/InlineError";
import { List, ListRow } from "../components/List";
import { Panel } from "../components/Panel";
import { Skeleton } from "../components/Skeleton";
import { environmentApi, environmentActionError, type CachedEnvironment } from "./environment-api";

export interface EnvironmentLibraryProps {
  revision: string;
  onCreate: (environment: CachedEnvironment) => void;
  onGetGame: () => void;
  onAccepted?: (origin?: MotionPoint) => void;
}

const api = environmentApi();

/** Only native, persisted recipes populate this list; failed refreshes retain known records. */
export function EnvironmentLibrary({ revision, onCreate, onGetGame, onAccepted }: EnvironmentLibraryProps) {
  const native = isTauriRuntime();
  const [entries, setEntries] = useState<CachedEnvironment[]>([]);
  const [skippedEntries, setSkippedEntries] = useState(0);
  const [loading, setLoading] = useState(native);
  const [error, setError] = useState(false);
  const [retry, setRetry] = useState(0);
  const errorId = useId();
  useEffect(() => {
    if (!native) return;
    let cancelled = false;
    setLoading(true); setError(false);
    void api.catalogue().then((next) => {
      if (!cancelled) { setEntries(next.entries); setSkippedEntries(next.skippedEntries); }
    }).catch(() => {
      if (!cancelled) setError(true);
    }).finally(() => {
      if (!cancelled) setLoading(false);
    });
    return () => { cancelled = true; };
  }, [native, revision, retry]);

  return <Panel title="已下载的游戏" description="使用前校验资源；Java和账户在启动时准备"
    actions={<Button size="sm" onClick={onGetGame} icon="download">获取游戏</Button>}>
    {error ? <InlineError id={errorId} message="未能读取已下载的游戏，请重试。">
      <Button size="sm" variant="quiet" onClick={() => setRetry((value) => value + 1)} pending={loading}>重试</Button>
    </InlineError> : null}
    {skippedEntries > 0 ? <p className="task__note" role="status">有{skippedEntries}份缓存清单未能读取，可重新下载对应版本。</p> : null}
    {loading && entries.length === 0 ? <Skeleton label="正在读取已下载的游戏" lines={3} /> : null}
    {!loading && !error && entries.length === 0 ? <EmptyState icon="download" title="还没有下载游戏"
      body={native ? "先获取一个游戏版本，之后可以用它创建多个实例。" : "打开桌面启动器后，获取游戏即可保存到本地。"} /> : null}
    {entries.length > 0 ? <List label="本地游戏缓存">
      {entries.map((entry) => <EnvironmentRow key={entry.id} entry={entry} onCreate={onCreate}
        onAccepted={onAccepted} onChanged={() => setRetry((value) => value + 1)} />)}
    </List> : null}
  </Panel>;
}

interface EnvironmentRowProps {
  entry: CachedEnvironment;
  onCreate: (environment: CachedEnvironment) => void;
  onChanged: () => void;
  onAccepted?: (origin?: MotionPoint) => void;
}

function EnvironmentRow({ entry, onCreate, onChanged, onAccepted }: EnvironmentRowProps) {
  const [pending, setPending] = useState<"pin" | "repair" | "remove" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirmRemove, setConfirmRemove] = useState(false);
  const busy = useRef(false);
  const errorId = useId();

  async function run(action: "pin" | "repair" | "remove", origin?: MotionPoint) {
    if (busy.current || (action === "remove" && entry.pinned)) return;
    busy.current = true;
    setPending(action);
    setError(null);
    try {
      if (action === "pin") await api.pin(entry.id, !entry.pinned);
      else if (action === "remove") {
        await api.remove(entry.id);
        setConfirmRemove(false);
      } else {
        await api.repair(entry.id);
        onAccepted?.(origin);
      }
      onChanged();
    } catch (failure) {
      setError(environmentActionError(action, failure));
    } finally {
      busy.current = false;
      setPending(null);
    }
  }

  return <ListRow>
    <div className="task__head"><span className="task__id task__name">Minecraft {entry.versionId}</span>
      <Button size="sm" disabled={pending !== null} onClick={() => onCreate(entry)}>用它创建实例</Button></div>
    <p className="task__phase">Windows x64 · {entry.resourceCount} 项资源 · {(entry.bytes / 1048576).toFixed(1)} MiB{entry.pinned ? " · 已固定保留" : ""}</p>
    <p className="task__note">缓存位置：<span className="download-destination" title={entry.path}>{entry.path}</span></p>
    <div className="task__foot">
      <Button size="sm" variant="quiet" aria-pressed={entry.pinned} disabled={pending !== null}
        pending={pending === "pin"} onClick={() => void run("pin")}>{entry.pinned ? "取消保留" : "固定保留"}</Button>
      <Button size="sm" variant="quiet" disabled={pending !== null} pending={pending === "repair"}
        onClick={(event) => {
          const rect = event.currentTarget.getBoundingClientRect();
          void run("repair", { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 });
        }}>校验并补齐</Button>
      <Button size="sm" variant="quiet" disabled={entry.pinned || pending !== null}
        title={entry.pinned ? "取消保留后可移除清单" : undefined}
        onClick={() => { setError(null); setConfirmRemove(true); }}>移除清单</Button>
    </div>
    {error && !confirmRemove ? <InlineError id={errorId} message={error} /> : null}
    <Dialog open={confirmRemove} title="移除缓存清单" onClose={() => { if (!busy.current) setConfirmRemove(false); }}
      actions={<><Button disabled={pending !== null} onClick={() => setConfirmRemove(false)}>取消</Button>
        <Button variant="danger" disabled={entry.pinned || pending !== null} pending={pending === "remove"}
          onClick={() => void run("remove")}>移除清单</Button></>}>
      <p>移除 Minecraft {entry.versionId} 的缓存清单？实例和共享资源会保留。</p>
      {error ? <InlineError id={errorId} message={error} /> : null}
    </Dialog>
  </ListRow>;
}

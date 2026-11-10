import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "../components/Button";
import type { InstalledGame } from "./game-install-api";
import { modrinthApi, type ModrinthProject, type ModVersions } from "./modrinth-api";
import { resourceSearchError } from "./resource-catalog-api";

const dependencyLabels = { required: "必需", optional: "可选", incompatible: "不兼容", embedded: "已内置" };
const versionLabels = { release: "正式版", beta: "测试版", alpha: "预览版" };
type Load<T> = { kind: "loading" } | { kind: "error"; message: string } | { kind: "ready"; value: T };

export function ModrinthProjectPanel({ projectId, game, onBack }: {
  projectId: string; game?: InstalledGame; onBack: () => void;
}) {
  const api = useMemo(() => modrinthApi(), []);
  const heading = useRef<HTMLHeadingElement>(null);
  const [project, setProject] = useState<Load<ModrinthProject>>({ kind: "loading" });
  const [versions, setVersions] = useState<Load<ModVersions>>({ kind: "loading" });
  const [reload, setReload] = useState(0);
  const [offset, setOffset] = useState(0);
  useEffect(() => { heading.current?.focus(); }, []);
  useEffect(() => {
    let active = true;
    setProject({ kind: "loading" });
    api.project(projectId).then(value => { if (active) setProject({ kind: "ready", value }); },
      error => { if (active) setProject({ kind: "error", message: resourceSearchError(error).message }); });
    return () => { active = false; };
  }, [api, projectId, reload]);
  // Parent keys this panel by project + instance selection. Each request also
  // has cleanup so late results cannot replace another page or selection.
  useEffect(() => {
    if (!game) return;
    let active = true;
    setVersions({ kind: "loading" });
    api.versions(projectId, game.id, offset).then(value => { if (active) setVersions({ kind: "ready", value }); },
      error => { if (active) setVersions({ kind: "error", message: resourceSearchError(error).message }); });
    return () => { active = false; };
  }, [api, projectId, game?.id, game?.version.gameVersion, game?.version.loader, game?.version.loaderVersion, offset, reload]);

  return <section className="mod-project" aria-labelledby="mod-project-heading">
    <Button size="sm" onClick={onBack}>返回搜索结果</Button>
    <h3 id="mod-project-heading" ref={heading} tabIndex={-1}>{project.kind === "ready" ? project.value.title : "模组详情"}</h3>
    {project.kind === "loading" ? <p role="status">正在读取详情…</p> : null}
    {project.kind === "error" ? <p role="alert">{project.message} <Button size="sm" onClick={() => setReload(n => n + 1)}>重试</Button></p> : null}
    {project.kind === "ready" ? <>
      <p className="mod-project__description">{project.value.description}</p>
      <a className="btn btn--secondary btn--sm" href={`https://modrinth.com/mod/${encodeURIComponent(project.value.slug)}`} target="_blank" rel="noopener noreferrer">在 Modrinth 查看 ↗</a>
      {project.value.clientSide === "unsupported" ? <p role="status">此项目仅用于服务器。</p> : null}
      {project.value.clientSide === "unknown" ? <p role="status">此项目尚未标明客户端支持情况。</p> : null}
    </> : null}
    {!game ? <p role="status">选择一个实例，查看对应版本。</p> : <>
      <h4>{game.name} · 对应版本</h4>
      {versions.kind === "loading" ? <p role="status">正在读取版本…</p> : null}
      {versions.kind === "error" ? <div role="alert"><p>{versions.message}</p><Button size="sm" onClick={() => setReload(n => n + 1)}>重试</Button></div> : null}
      {versions.kind === "ready" ? <>
        <p className="resource-card__meta">{versions.value.target.gameVersion} · {versions.value.target.loader} · {versions.value.total} 个版本</p>
        {versions.value.target.loader === "vanilla" ? <p>此实例是原版游戏。请选择已安装模组加载器的实例。</p> : versions.value.total === 0 ? <p role="status">没有适用于此实例的客户端版本。</p> : null}
        <ul className="mod-version-list" aria-label="对应版本">
          {versions.value.versions.map(version => <li key={version.versionId}>
            <details>
              <summary>{version.name} <span className="resource-card__meta">{versionLabels[version.kind]}</span></summary>
              <p className="resource-card__meta">{version.versionNumber} · {version.published.slice(0, 10)}</p>
              <ul aria-label="版本文件">{version.files.map((file, i) => <li key={`${file.name}-${i}`}>{file.name} · {(file.bytes / 1024 ** 2).toFixed(2)} MiB{file.primary ? " · 主文件" : ""}</li>)}</ul>
              <h5>依赖</h5>
              {version.dependencies.length ? <ul aria-label="版本依赖">{version.dependencies.map((dep, i) => <li key={i}>
                <span>{dependencyLabels[dep.kind]} · </span>
                {dep.projectId ? <a href={`https://modrinth.com/mod/${encodeURIComponent(dep.projectId)}`} target="_blank" rel="noopener noreferrer">{dep.projectId} ↗</a> : dep.fileName ?? "未标明项目"}
                {dep.versionId ? <span> · 指定版本 {dep.versionId}</span> : null}
              </li>)}</ul> : <p>项目未声明依赖。</p>}
              <a className="btn btn--secondary btn--sm" href={`https://modrinth.com/mod/${encodeURIComponent(projectId)}/version/${encodeURIComponent(version.versionId)}`} target="_blank" rel="noopener noreferrer">查看版本 ↗</a>
            </details>
          </li>)}
        </ul>
        {versions.value.total > 20 ? <nav className="mod-version-pager" aria-label="版本分页">
          <Button size="sm" disabled={offset === 0} onClick={() => setOffset(n => Math.max(0, n - 20))}>上一页</Button>
          <span>{Math.floor(offset / 20) + 1} / {Math.ceil(versions.value.total / 20)}</span>
          <Button size="sm" disabled={offset + 20 >= versions.value.total} onClick={() => setOffset(n => n + 20)}>下一页</Button>
        </nav> : null}
      </> : null}
    </>}
  </section>;
}

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { Button } from "../components/Button";
import { EmptyState } from "../components/EmptyState";
import type { InstalledGame } from "./game-install-api";
import { ModrinthProjectPanel } from "./ModrinthProjectPanel";
import {
  RESOURCE_PROVIDERS,
  resourceCatalogApi,
  resourceSearchError,
  supportsCoreSearch,
  type ResourceProject,
  type ResourceProvider,
  type ResourceTarget,
} from "./resource-catalog-api";

export const RESOURCE_CATEGORIES = [
  { id: "mods", label: "Mod", href: "https://www.curseforge.com/minecraft/mc-mods", link: "浏览 CurseForge Mod" },
  { id: "modpacks", label: "整合包", href: "https://modrinth.com/modpacks", link: "浏览 Modrinth 整合包" },
  { id: "datapacks", label: "数据包", href: "https://modrinth.com/datapacks", link: "浏览 Modrinth 数据包" },
  { id: "packs", label: "资源包", href: "https://modrinth.com/resourcepacks", link: "浏览 Modrinth 资源包" },
  { id: "shaders", label: "光影包", href: "https://modrinth.com/shaders", link: "浏览 Modrinth 光影包" },
] as const;
export type ResourceCategory = typeof RESOURCE_CATEGORIES[number]["id"];

export interface ResourceCatalogProps {
  category: ResourceCategory;
  /** Compatible target from the selected instance, when available. */
  target?: ResourceTarget;
  games?: InstalledGame[];
}

type SearchState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ready"; projects: ResourceProject[] }
  | { kind: "error"; message: string; code?: string; retryable: boolean };

/**
 * Provider-scoped content search and Modrinth detail/version reads. Installation
 * is a separate transaction; browsing never writes an instance.
 * The community link stays available regardless of search state. Categories
 * the core cannot search yet retain an explicit unavailable state and links.
 */
export function ResourceCatalog({ category, target: fallbackTarget, games = [] }: ResourceCatalogProps) {
  const item = RESOURCE_CATEGORIES.find((entry) => entry.id === category)!;
  const searchable = supportsCoreSearch(category);
  const [provider, setProvider] = useState<ResourceProvider>(RESOURCE_PROVIDERS[0].id);
  const [query, setQuery] = useState("");
  const [state, setState] = useState<SearchState>({ kind: "idle" });
  const requestRef = useRef(0);
  const [instanceId, setInstanceId] = useState("");
  const [projectId, setProjectId] = useState<string>();
  const searchInput = useRef<HTMLInputElement>(null);
  const restoreSearchFocus = useRef(false);
  const game = games.find(item => item.id === instanceId);
  const target = game?.version ?? fallbackTarget;

  // Stable API instance so the memoized `search` does not churn every render.
  const api = useMemo(() => resourceCatalogApi(), []);

  const gameVersion = target?.gameVersion;
  const loader = target?.loader;
  const loaderVersion = target?.loaderVersion;

  const search = useCallback(async () => {
    if (!searchable) return;
    const token = ++requestRef.current;
    setState({ kind: "loading" });
    try {
      const page = await api.search(
        provider,
        category,
        query,
        gameVersion ? { gameVersion, loader: loader ?? "vanilla", ...(loaderVersion ? { loaderVersion } : {}) } : undefined,
      );
      if (token !== requestRef.current) return;
      setState({ kind: "ready", projects: page.projects });
    } catch (error) {
      if (token !== requestRef.current) return;
      const mapped = resourceSearchError(error);
      setState({ kind: "error", message: mapped.message, code: mapped.code, retryable: mapped.retryable });
    }
  }, [api, searchable, provider, category, query, gameVersion, loader, loaderVersion]);

  // Invalidate in-flight requests on any provider/category/target change and on
  // unmount so stale responses are never applied.
  useEffect(() => {
    return () => {
      requestRef.current += 1;
    };
  }, []);

  useEffect(() => {
    requestRef.current += 1;
    setState({ kind: "idle" });
  }, [provider, category, gameVersion, loader, loaderVersion, query]);

  useEffect(() => { setProjectId(undefined); }, [provider, category]);
  useLayoutEffect(() => {
    if (!projectId && restoreSearchFocus.current) {
      restoreSearchFocus.current = false;
      searchInput.current?.focus();
    }
  }, [projectId]);

  const compatibility = gameVersion
    ? `${gameVersion}${loader && loader !== "vanilla" ? ` · ${loader}` : ""}${loaderVersion ? ` · ${loaderVersion}` : ""}`
    : undefined;

  return <section className="catalog-surface" aria-label={`${item.label}资源`}>
    {searchable ? <label className="install-field catalog-instance">目标实例
      <select value={game?.id ?? ""} onChange={event => setInstanceId(event.target.value)}>
        <option value="">全部项目 · 未选择实例</option>
        {games.map(instance => <option key={instance.id} value={instance.id}>{instance.name} · {instance.version.gameVersion} · {instance.version.loader}</option>)}
      </select>
    </label> : null}
    {projectId ? <ModrinthProjectPanel key={`${projectId}:${game?.id}:${game?.version.gameVersion}:${game?.version.loader}:${game?.version.loaderVersion}`} projectId={projectId} game={game}
      onBack={() => { restoreSearchFocus.current = true; setProjectId(undefined); }} /> : <>
    {searchable ?
      <div className="catalog-search">
        <label className="install-field catalog-search__provider">来源
          <select value={provider} onChange={(event) => setProvider(event.target.value as ResourceProvider)}>
            {RESOURCE_PROVIDERS.map((entry) => <option key={entry.id} value={entry.id}>{entry.label}</option>)}
          </select>
        </label>
        <label className="install-field catalog-search__query">搜索{item.label}
          <input ref={searchInput} type="search" maxLength={256} value={query} onChange={(event) => setQuery(event.target.value)} placeholder="名称或关键词" onKeyDown={(event) => { if (event.key === "Enter" && state.kind !== "loading") void search(); }} />
        </label>
        <Button size="sm" variant="primary" pending={state.kind === "loading"} onClick={() => void search()}>搜索</Button>
      </div> : null}

    {!searchable ? <p role="status" className="catalog-state">该分类暂不支持站内搜索，请使用下方社区链接浏览。</p> : null}
    {compatibility ? <p className="catalog-compatibility">兼容目标：{compatibility}</p> : null}
    {loader === "vanilla" ? <p className="catalog-state">原版实例无法加载 Mod。请选择已安装模组加载器的实例。</p> : null}

    {state.kind === "loading" ? <p role="status" className="catalog-state">正在搜索…</p> : null}
    {state.kind === "error" ? <div role="alert" className="catalog-state">
      <p>{state.message}</p>
      {state.retryable ? <Button size="sm" onClick={() => void search()}>重试</Button> : null}
    </div> : null}
    {state.kind === "ready" && state.projects.length === 0 ? <EmptyState icon="layers" title="没有匹配项目" body="换个关键词，或在社区网站继续浏览。" /> : null}
    {state.kind === "ready" && state.projects.length > 0 ? <ul className="resource-results" aria-label="搜索结果">
      {state.projects.map((project) => <li key={`${project.provider}:${project.projectId}`} className="resource-card">
        <div className="resource-card__content">
          <h3>{project.title}</h3>
          <p className="resource-card__meta">来源：{project.provider}</p>
        </div>
        <div className="resource-card__actions">
          {project.provider === "modrinth" ? <Button size="sm" onClick={() => setProjectId(project.projectId)}>详情与版本</Button>
            : project.externalUrl ? <a className="btn btn--secondary btn--sm" href={project.externalUrl} target="_blank" rel="noopener noreferrer">查看详情<span aria-hidden="true"> ↗</span></a> : null}
        </div>
      </li>)}
    </ul> : null}
    {state.kind === "ready" && state.projects.length === 20 ? <p className="resource-card__meta">显示前 20 个结果，可缩小关键词或选择目标实例。</p> : null}
    {searchable && state.kind === "idle" ? <EmptyState icon="layers" title={`${item.label}资源`} body="选择来源并搜索，或先在社区网站查看资源与兼容版本。" /> : null}

    <div className="resource-links"><a className="btn btn--secondary btn--md" href={item.href} target="_blank" rel="noopener noreferrer">{item.link}<span aria-hidden="true"> ↗</span></a></div>
    </>}
  </section>;
}

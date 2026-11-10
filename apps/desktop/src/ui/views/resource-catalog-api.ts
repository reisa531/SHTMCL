import { dispatchCoreRead } from "../../bridge";
import type { CommandRequest, CommandResponse } from "../../contract";
import type { ResourceCategory } from "./ResourceCatalog";

/** Provider ids the UI may request; the backend stays authoritative on support. */
export const RESOURCE_PROVIDERS = [
  { id: "modrinth", label: "Modrinth" },
  { id: "curseforge", label: "CurseForge" },
  { id: "local", label: "本地" },
] as const;
export type ResourceProvider = typeof RESOURCE_PROVIDERS[number]["id"];

const PROVIDER_IDS: readonly string[] = RESOURCE_PROVIDERS.map((entry) => entry.id);

/** The actual core `content.search` contract has no category support yet; only
 * mods can be searched through the core. Other categories stay link-only. */
export const CORE_SEARCHABLE_CATEGORIES: readonly ResourceCategory[] = ["mods"];

export function supportsCoreSearch(category: ResourceCategory): boolean {
  return CORE_SEARCHABLE_CATEGORIES.includes(category);
}

export interface ResourceTarget {
  gameVersion: string;
  loader: string;
  loaderVersion?: string;
}

/** Only populated once it can be built from a trusted provider + identifier. */
export interface ResourceProject {
  provider: ResourceProvider;
  projectId: string;
  slug: string;
  title: string;
  externalUrl?: string;
}

export interface ResourceSearchResult {
  queryId: string;
  projects: ResourceProject[];
}

/** `content.owner.unavailable` means the content module is not wired yet. */
export const CONTENT_OWNER_UNAVAILABLE = "content.owner.unavailable";
/** No dispatch can run outside the desktop host (browser preview). */
export const BROWSER_RUNTIME_UNAVAILABLE = "ui.browser.runtime_unavailable";

type Dispatch = (request: CommandRequest) => Promise<CommandResponse>;

function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Invalid core response");
  return value as Record<string, unknown>;
}

function isProvider(value: unknown): value is ResourceProvider {
  return typeof value === "string" && PROVIDER_IDS.includes(value);
}

/** Error codes surfaced by the bridge are preserved so the UI can react. */
function errorCode(error: unknown): string | undefined {
  if (!error || typeof error !== "object") return undefined;
  const code = (error as { code?: unknown }).code;
  return typeof code === "string" ? code : undefined;
}

/** The bridge throws a plain Error in the browser preview; detect that case
 * without inventing a code for genuine desktop failures. */
function looksLikeBrowserRuntime(error: unknown): boolean {
  return error instanceof Error && /desktop runtime|Native runtime required|requires the desktop/i.test(error.message);
}

function unknownProviderError(): Error & { code: string } {
  const error = new Error("Unknown provider") as Error & { code: string };
  error.code = "ui.content.unknown_provider";
  return error;
}

export function deriveExternalUrl(provider: ResourceProvider, projectId: string, slug: string): string | undefined {
  const identity = slug || (provider === "modrinth" ? projectId : "");
  if (!identity || identity === "." || identity === "..") return undefined;
  const encoded = encodeURIComponent(identity);
  switch (provider) {
    case "modrinth":
      return `https://modrinth.com/mod/${encoded}`;
    case "curseforge":
      return `https://www.curseforge.com/minecraft/mc-mods/${encoded}`;
    case "local":
      return undefined;
    default:
      return undefined;
  }
}

export function resourceCatalogApi(read: Dispatch = dispatchCoreRead) {
  function request(payload: Record<string, unknown>): CommandRequest {
    return { requestId: crypto.randomUUID(), payload: { command: "content.search", ...payload } };
  }

  return {
    /**
     * Real content search. The request carries the provider, query, a strict
     * nested target (game version / loader / optional loader version) and a
     * limit; no category is sent because the core contract has none. Results
     * are inspect-only.
     */
    async search(
      provider: ResourceProvider,
      category: ResourceCategory,
      query: string,
      target?: ResourceTarget,
      limit = 20,
    ): Promise<ResourceSearchResult> {
      if (!supportsCoreSearch(category)) {
        const error = new Error("core search unavailable for category") as Error & { code: string };
        error.code = CONTENT_OWNER_UNAVAILABLE;
        throw error;
      }
      if (!isProvider(provider)) throw unknownProviderError();

      const queryId = crypto.randomUUID();
      const payload: Record<string, unknown> = {
        queryId,
        provider,
        query: query.trim(),
        ...(target ? { target: {
          gameVersion: target.gameVersion,
          loader: target.loader,
          ...(target.loaderVersion ? { loaderVersion: target.loaderVersion } : {}),
        } } : {}),
        limit: Number.isSafeInteger(limit) && limit > 0 ? Math.min(limit, 100) : 20,
      };

      let response: CommandResponse;
      try {
        response = await read(request(payload));
      } catch (error) {
        if (looksLikeBrowserRuntime(error)) {
          const wrapped = new Error("requires desktop") as Error & { code: string };
          wrapped.code = BROWSER_RUNTIME_UNAVAILABLE;
          throw wrapped;
        }
        throw error;
      }

      if (response.type !== "completed") throw new Error("Expected completed response");
      const result = object(response.result);

      // `connected` must be strictly true; anything else means the service is
      // unavailable, never an empty result set.
      if (result.connected !== true) {
        const error = new Error("Search service unavailable") as Error & { code: string };
        error.code = "ui.content.service_unavailable";
        throw error;
      }
      // Validate the echoed queryId so a stale or mismatched response is not
      // applied to this request.
      if (result.queryId !== queryId) {
        const error = new Error("Search response identity mismatch") as Error & { code: string };
        error.code = "ui.content.stale_query";
        throw error;
      }
      if (!Array.isArray(result.projects)) throw new Error("Invalid content search page");

      const projects = result.projects.map((item) => {
        const entry = object(item);
        if (!isProvider(entry.provider)) throw new Error("Invalid project provider");
        if (entry.provider !== provider) throw new Error("Project provider mismatch");
        if (typeof entry.projectId !== "string" || !entry.projectId) throw new Error("Invalid project id");
        if (typeof entry.slug !== "string") throw new Error("Invalid project slug");
        if (typeof entry.title !== "string" || !entry.title) throw new Error("Invalid project title");
        const project: ResourceProject = {
          provider: entry.provider,
          projectId: entry.projectId,
          slug: entry.slug,
          title: entry.title,
        };
        const externalUrl = deriveExternalUrl(entry.provider, entry.projectId, entry.slug);
        if (externalUrl) project.externalUrl = externalUrl;
        return project;
      });

      // identities describes provider authentication, independently of the
      // number of projects. It is not needed for read-only project rendering.

      return { queryId, projects };
    },
  };
}

/** Maps a raw search failure to concise, honest, retryable UI copy. */
export function resourceSearchError(error: unknown): { code?: string; message: string; retryable: boolean } {
  const code = errorCode(error);
  const messages: Record<string, string> = {
    "content.modrinth.rate_limited": "请求较多，请稍后再试。",
    "content.modrinth.busy": "正在读取其他资源，请稍后再试。",
    "content.modrinth.not_found": "此项目已下架或不存在。",
    "content.modrinth.invalid_metadata": "资源信息不完整，请稍后重试。",
    "content.modrinth.unavailable": "暂时无法连接 Modrinth，请重试。",
    "content.source.unavailable": "暂时无法连接资源来源，请重试。",
  };
  if (code && messages[code]) return { code, message: messages[code], retryable: code !== "content.modrinth.not_found" };
  if (code === CONTENT_OWNER_UNAVAILABLE) {
    return { code, message: "搜索服务尚未接入", retryable: true };
  }
  if (code === BROWSER_RUNTIME_UNAVAILABLE) {
    return { code, message: "搜索需要桌面版运行环境", retryable: true };
  }
  if (code === "ui.content.service_unavailable") {
    return { code, message: "搜索服务当前不可用，请稍后重试", retryable: true };
  }
  if (code === "ui.content.stale_query") {
    return { code, message: "搜索结果已过期，请重试。", retryable: true };
  }
  if (code === "ui.content.unknown_provider") {
    return { code, message: "未知的来源，请重新选择。", retryable: true };
  }
  return { code, message: "未能加载资源，请重试。", retryable: true };
}


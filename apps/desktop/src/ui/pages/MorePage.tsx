import { Button } from "../components/Button";
import { EmptyState } from "../components/EmptyState";
import { PageHeader } from "../views/PageHeader";
import type { PageId } from "../../ux/copy";

export function MorePage({ onNavigate }: { onNavigate: (page: PageId) => void }) {
  return <>
    <PageHeader title="更多" />
    <section className="catalog-surface" aria-label="服务器目录">
      <EmptyState icon="users" title="服务器" body="服务器目录尚未接入。已有房间与加入入口位于联机页。" />
      <div className="resource-links"><Button onClick={() => onNavigate("multiplayer")}>打开联机</Button></div>
    </section>
    <div className="download-areas"><Button onClick={() => onNavigate("settings")}>设置与关于</Button><Button onClick={() => onNavigate("tasks")}>查看任务与结果</Button></div>
  </>;
}
